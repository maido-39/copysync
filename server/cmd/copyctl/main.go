// Command copyctl is a headless reference/CLI client for a CopySync server. It
// speaks the same wire protocol as the (planned) desktop and mobile clients —
// pairing with OTP + SPKI certificate pinning, the WebSocket control channel,
// and the HTTPS blob channel — and doubles as the protocol conformance harness.
package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/privacy"
	"github.com/syaro/copysync/internal/protocol"
)

func main() {
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}
	var err error
	switch os.Args[1] {
	case "pair":
		err = cmdPair(os.Args[2:])
	case "send":
		err = cmdSend(os.Args[2:])
	case "pull":
		err = cmdPull(os.Args[2:])
	case "discover":
		err = cmdDiscover(os.Args[2:])
	case "watch":
		err = cmdWatch(os.Args[2:])
	case "run":
		err = cmdRun(os.Args[2:])
	case "pool":
		err = cmdPool(os.Args[2:])
	case "history":
		err = cmdHistory(os.Args[2:])
	case "-h", "--help", "help":
		usage()
		return
	default:
		fmt.Fprintf(os.Stderr, "unknown command %q\n\n", os.Args[1])
		usage()
		os.Exit(2)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprint(os.Stderr, `copyctl — CopySync reference CLI client

Usage:
  copyctl pair    --server URL --otp CODE --name NAME [--pin B64] [--config FILE]
  copyctl send    [--text STR | --file PATH] [--targets all|id,id] [--config FILE]
  copyctl discover [--timeout 4s]
  copyctl pull    --id sha256:HEX --out PATH [--config FILE]
  copyctl watch   [--save-dir DIR] [--config FILE]
  copyctl run     [--targets all|id,id] [--save-dir DIR] [--config FILE]
  copyctl history [--search TERM]
  copyctl pool    NAME [--config FILE]

Commands:
  pair     Redeem an OTP and store the device token + server pin.
  send     Send a clip. Text/small files upload eagerly; files over the server
           threshold are advertised on demand and served while this stays running.
  pull     Download a blob by id (triggers an on-demand pull from the source).
  watch    Connect and print/save every incoming clip (no clipboard needed).
  run      Two-way sync with the OS clipboard (Wayland/X11; headless = receive only).
  history  Show or search the local clipboard log.
`)
}

func cmdPair(args []string) error {
	fs := flag.NewFlagSet("pair", flag.ExitOnError)
	server := fs.String("server", "", "server base URL, e.g. https://192.168.1.10:8443")
	otp := fs.String("otp", "", "one-time pairing code")
	name := fs.String("name", "", "this device's unique name")
	pin := fs.String("pin", "", "server SPKI pin (base64); empty = trust on first use")
	e2ePass := fs.String("e2e-pass", "", "optional E2E passphrase; enables client-side encryption")
	cfgPath := fs.String("config", defaultConfigPath(), "config file path")
	_ = fs.Parse(args)
	if *server == "" || *otp == "" || *name == "" {
		return fmt.Errorf("--server, --otp and --name are required")
	}
	usedPin := *pin
	if usedPin == "" {
		si, err := fetchServerInfoInsecure(*server)
		if err != nil {
			return fmt.Errorf("could not fetch server info: %w", err)
		}
		usedPin = si.SPKIPin
		fmt.Printf("warning: trusting server pin on first use: %s\n", usedPin)
	}
	hc, err := pinnedHTTPClient(usedPin)
	if err != nil {
		return err
	}
	cfg, err := claimPairing(hc, *server, *otp, *name)
	if err != nil {
		return err
	}
	cfg.Pin = usedPin
	cfg.E2EPass = *e2ePass
	if err := saveConfig(*cfgPath, cfg); err != nil {
		return err
	}
	fmt.Printf("paired as %q (%s) with server %q\nconfig saved to %s\n", cfg.DeviceName, cfg.DeviceID, cfg.ServerName, *cfgPath)
	return nil
}

func cmdSend(args []string) error {
	fs := flag.NewFlagSet("send", flag.ExitOnError)
	cfgPath := fs.String("config", defaultConfigPath(), "config file path")
	text := fs.String("text", "", "text to send")
	file := fs.String("file", "", "file to send via the blob channel")
	targetsArg := fs.String("targets", "all", `"all" or comma-separated device ids`)
	_ = fs.Parse(args)
	cfg, err := loadConfig(*cfgPath)
	if err != nil {
		return fmt.Errorf("load config (run `copyctl pair` first): %w", err)
	}
	cl, err := newClient(cfg, openHistory(historyPathFor(*cfgPath)))
	if err != nil {
		return err
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	conn, hello, err := cl.connect(ctx)
	if err != nil {
		return err
	}
	defer conn.CloseNow()
	targets := parseTargets(*targetsArg)

	if *file != "" {
		st, err := os.Stat(*file)
		if err != nil {
			return err
		}
		size := st.Size()
		// Over the server threshold → advertise on demand (no upload) and stay to serve.
		if hello.OnDemandThreshold > 0 && size > hello.OnDemandThreshold {
			bid, err := cl.sendLazyClip(ctx, conn, *file, size, targets)
			if err != nil {
				return err
			}
			cl.hist.append("out", cfg.DeviceID, "(file on-demand) "+filepath.Base(*file), string(bid))
			fmt.Printf("sent %s on demand (%d bytes > %d threshold) as %s\n", filepath.Base(*file), size, hello.OnDemandThreshold, bid)
			fmt.Println("holding the file; serving when another device requests it. Ctrl-C to stop.")
			if err := cl.serveLoop(ctx, conn); err != nil && ctx.Err() == nil {
				return err
			}
			return nil
		}
		// Small file → eager upload.
		data, err := os.ReadFile(*file)
		if err != nil {
			return err
		}
		id, err := cl.sendBlob(ctx, conn, data, mimeOf(*file), filepath.Base(*file), targets)
		if err != nil {
			return err
		}
		cl.hist.append("out", cfg.DeviceID, "(file) "+filepath.Base(*file), "")
		ack, err := cl.waitAck(ctx, conn, id)
		if err != nil {
			return err
		}
		fmt.Printf("sent %s → status=%s queuedFor=%v\n", id, ack.Status, ack.QueuedFor)
		return nil
	}

	if *text != "" {
		// Explicit sends are never blocked (deliberate user action — matches the
		// desktop's SendText semantics), but warn when the clip looks sensitive.
		if !cfg.PrivacyFilterOff {
			res, _ := privacy.CompilePatterns(cfg.CustomPatterns)
			if why := privacy.Classify(*text, res); why != 0 {
				fmt.Fprintf(os.Stderr, "warning: clip classifies as sensitive (%s) — sending anyway (explicit send)\n", why.Label())
			}
		}
		id, err := cl.sendText(ctx, conn, *text, targets)
		if err != nil {
			return err
		}
		cl.hist.append("out", cfg.DeviceID, *text, "")
		ack, err := cl.waitAck(ctx, conn, id)
		if err != nil {
			return err
		}
		fmt.Printf("sent %s → status=%s queuedFor=%v\n", id, ack.Status, ack.QueuedFor)
		return nil
	}
	return fmt.Errorf("provide --text or --file")
}

func cmdPull(args []string) error {
	fs := flag.NewFlagSet("pull", flag.ExitOnError)
	cfgPath := fs.String("config", defaultConfigPath(), "config file path")
	id := fs.String("id", "", "blob id (sha256:<hex>)")
	out := fs.String("out", "", "output file path")
	_ = fs.Parse(args)
	if *id == "" || *out == "" {
		return fmt.Errorf("--id and --out are required")
	}
	cfg, err := loadConfig(*cfgPath)
	if err != nil {
		return fmt.Errorf("load config (run `copyctl pair` first): %w", err)
	}
	cl, err := newClient(cfg, nil)
	if err != nil {
		return err
	}
	fmt.Printf("requesting %s (server pulls from the source device on demand)…\n", *id)
	data, code, err := cl.pinnedFetch(model.BlobID(*id), 120*time.Second)
	if err != nil {
		return fmt.Errorf("pull failed (HTTP %d): %w", code, err)
	}
	if cl.key != nil {
		pt, derr := open(cl.key, data)
		if derr != nil {
			return fmt.Errorf("decrypt failed (wrong passphrase?): %w", derr)
		}
		data = pt
	}
	if err := os.WriteFile(*out, data, 0o644); err != nil {
		return err
	}
	sum := sha256.Sum256(data)
	fmt.Printf("downloaded %d bytes → %s (sha256 %s)\n", len(data), *out, hex.EncodeToString(sum[:]))
	return nil
}

func cmdWatch(args []string) error {
	fs := flag.NewFlagSet("watch", flag.ExitOnError)
	cfgPath := fs.String("config", defaultConfigPath(), "config file path")
	saveDir := fs.String("save-dir", ".", "directory to save received files")
	exitAfter := fs.Int("exit-after", 0, "exit after receiving N clips (0 = until interrupted)")
	_ = fs.Parse(args)
	cfg, err := loadConfig(*cfgPath)
	if err != nil {
		return fmt.Errorf("load config (run `copyctl pair` first): %w", err)
	}
	cl, err := newClient(cfg, openHistory(historyPathFor(*cfgPath)))
	if err != nil {
		return err
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	conn, ok, err := cl.connect(ctx)
	if err != nil {
		return err
	}
	defer conn.CloseNow()
	fmt.Printf("watching as %q on %q (%d device(s) known). Ctrl-C to stop.\n", cfg.DeviceName, cfg.ServerName, len(ok.Roster))
	count := 0
	for {
		env, err := readMsg(ctx, conn)
		if err != nil {
			if ctx.Err() != nil {
				return nil
			}
			return err
		}
		switch env.T {
		case protocol.TypeClip:
			var ev model.ClipEvent
			if env.Decode(&ev) == nil {
				fmt.Println(cl.handleIncoming(ev, *saveDir))
				count++
				if *exitAfter > 0 && count >= *exitAfter {
					return nil
				}
			}
		case protocol.TypePresence:
			var p protocol.Presence
			if env.Decode(&p) == nil {
				state := "offline"
				if p.Online {
					state = "online"
				}
				fmt.Printf("· %s is now %s\n", p.Device.Name, state)
			}
		case protocol.TypeTokenRotate:
			var tr protocol.TokenRotate
			if env.Decode(&tr) == nil && tr.Token != "" {
				cfg.Token = tr.Token
				cl.cfg.Token = tr.Token
				if err := saveConfig(*cfgPath, cfg); err != nil {
					fmt.Printf("! token rotated but save failed: %v\n", err)
				} else {
					fmt.Println("· bearer token rotated + saved")
				}
			}
		}
	}
}

func cmdRun(args []string) error {
	fs := flag.NewFlagSet("run", flag.ExitOnError)
	cfgPath := fs.String("config", defaultConfigPath(), "config file path")
	saveDir := fs.String("save-dir", ".", "directory to save received files")
	targetsArg := fs.String("targets", "all", `"all" or comma-separated device ids`)
	_ = fs.Parse(args)
	cfg, err := loadConfig(*cfgPath)
	if err != nil {
		return fmt.Errorf("load config (run `copyctl pair` first): %w", err)
	}
	cl, err := newClient(cfg, openHistory(historyPathFor(*cfgPath)))
	if err != nil {
		return err
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	conn, _, err := cl.connect(ctx)
	if err != nil {
		return err
	}
	defer conn.CloseNow()

	cb := detectClipboard()
	targets := parseTargets(*targetsArg)
	fmt.Printf("syncing clipboard with %q using the %s backend. Ctrl-C to stop.\n", cfg.ServerName, cb.Name())

	// Incoming: write text clips to the clipboard; save file clips.
	go func() {
		for {
			env, err := readMsg(ctx, conn)
			if err != nil {
				return
			}
			if env.T != protocol.TypeClip {
				continue
			}
			var ev model.ClipEvent
			if env.Decode(&ev) != nil {
				continue
			}
			if ev.InlineText != "" {
				cl.echo.markWritten(ev.Sha256)
				_ = cb.Write(ev.InlineText)
			}
			fmt.Println(cl.handleIncoming(ev, *saveDir))
		}
	}()

	// Privacy filter (parity with the desktop/Android clients): captured clips
	// that classify as sensitive are not synced. Deliberately NOT recorded in
	// history.jsonl either — unlike the desktop's encrypted+purged history,
	// copyctl's log is plaintext, so writing the secret there would defeat the
	// point of blocking it.
	customRes, patErrs := privacy.CompilePatterns(cfg.CustomPatterns)
	for _, e := range patErrs {
		fmt.Fprintln(os.Stderr, "warning: bad customPatterns regex ignored:", e)
	}

	// Outgoing: broadcast local clipboard changes (suppressing our own writes).
	cb.Watch(ctx, func(text string) {
		sum := sha256.Sum256([]byte(text))
		sha := hex.EncodeToString(sum[:])
		if cl.echo.seen(sha) {
			return
		}
		if !cfg.PrivacyFilterOff {
			if why := privacy.Classify(text, customRes); why != 0 {
				fmt.Fprintf(os.Stderr, "blocked by privacy filter (%s) — not synced; set privacyFilterOff in the config to disable\n", why.Label())
				return
			}
		}
		if _, err := cl.sendText(ctx, conn, text, targets); err != nil {
			fmt.Fprintln(os.Stderr, "send failed:", err)
			return
		}
		cl.hist.append("out", cfg.DeviceID, text, "")
	})
	return nil
}

func cmdHistory(args []string) error {
	fs := flag.NewFlagSet("history", flag.ExitOnError)
	term := fs.String("search", "", "substring to filter by")
	cfgPath := fs.String("config", defaultConfigPath(), "config file path (history sits beside it)")
	_ = fs.Parse(args)
	entries, err := openHistory(historyPathFor(*cfgPath)).search(*term)
	if err != nil {
		return err
	}
	if len(entries) == 0 {
		fmt.Println("(no history)")
		return nil
	}
	for _, e := range entries {
		arrow := "←"
		if e.Dir == "out" {
			arrow = "→"
		}
		text := e.Text
		if e.Blob != "" && text == "" {
			text = "(blob " + e.Blob + ")"
		}
		fmt.Printf("%s %s %s %s\n", e.TS.Format(time.RFC3339), arrow, e.Origin, strings.ReplaceAll(text, "\n", "\\n"))
	}
	return nil
}

// historyPathFor places the local history log next to a client's config file,
// so distinct --config files keep separate histories.
func historyPathFor(cfgPath string) string {
	return filepath.Join(filepath.Dir(cfgPath), "history.jsonl")
}

// handleIncoming records a received clip in history and returns a printable line.
func (c *Client) handleIncoming(ev model.ClipEvent, saveDir string) string {
	if ev.InlineText != "" {
		text := ev.InlineText
		if ev.Enc != nil {
			pt, ok := c.decryptText(ev)
			if !ok {
				return fmt.Sprintf("[%s] [e2e ciphertext — %s]", ev.OriginDevice, c.e2eWhy(ev))
			}
			text = pt
		}
		c.hist.append("in", string(ev.OriginDevice), text, "")
		return fmt.Sprintf("[%s] text: %s", ev.OriginDevice, text)
	}
	if ev.BlobID != "" {
		data, err := c.getBlob(ev.BlobID)
		if err != nil {
			return fmt.Sprintf("[%s] blob %s (download failed: %v)", ev.OriginDevice, ev.BlobID, err)
		}
		sum := sha256.Sum256(data)
		gotHex := hex.EncodeToString(sum[:])
		wantHex := strings.TrimPrefix(string(ev.BlobID), "sha256:")
		integrity := "ok"
		if gotHex != wantHex {
			integrity = "HASH MISMATCH"
		}
		dec := ""
		if ev.Enc != nil {
			pt, ok := c.decryptBytes(ev, data)
			if !ok {
				return fmt.Sprintf("[%s] blob %d bytes [e2e — %s]", ev.OriginDevice, len(data), c.e2eWhy(ev))
			}
			data = pt
			dec = " (decrypted)"
		}
		_ = os.MkdirAll(saveDir, 0o755)
		path := filepath.Join(saveDir, wantHex+".blob")
		_ = os.WriteFile(path, data, 0o644)
		c.hist.append("in", string(ev.OriginDevice), "(blob) "+path, string(ev.BlobID))
		return fmt.Sprintf("[%s] blob %d bytes → %s (%s)%s", ev.OriginDevice, len(data), path, integrity, dec)
	}
	return fmt.Sprintf("[%s] (empty clip)", ev.OriginDevice)
}
