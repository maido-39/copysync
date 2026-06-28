// Package transport implements the WebSocket control channel: it upgrades the
// connection, performs the hello/token handshake, and bridges the socket to the
// hub via a read pump and a write pump.
package transport

import (
	"context"
	"log/slog"
	"net/http"
	"time"

	"github.com/coder/websocket"
	"github.com/syaro/copysync/internal/hub"
	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/protocol"
)

const (
	sendBuffer   = 64
	writeTimeout = 10 * time.Second
	helloTimeout = 10 * time.Second
	pingInterval = 30 * time.Second
	// idleTimeout bounds how long a read may block with no inbound frame. It is
	// deliberately larger than pingInterval so a healthy idle peer (which still
	// answers pings, but those are handled by the library and do not surface as
	// reads) is never reaped; a dead peer is detected within idleTimeout instead
	// of the ~40s zombie window the old unbounded read allowed.
	idleTimeout = 75 * time.Second
	// slowWriteThreshold is the fraction of writeTimeout past which a write is
	// logged (in debug mode) as "slow" — an early warning of a stalling peer.
	slowWriteThreshold = writeTimeout / 2
)

// Deps are the dependencies of the WS handler.
type Deps struct {
	Hub *hub.Hub
	Log *slog.Logger
	Now func() time.Time
	// ValidateToken returns the device for a (deviceID, token) pair, or false.
	ValidateToken func(model.DeviceID, string) (model.Device, bool)
	// MaybeRotateToken optionally re-issues the device's bearer token after a
	// successful auth, returning a new plaintext token to deliver (or "" for none).
	MaybeRotateToken func(model.DeviceID, string) string
	// MaxMessage returns the current WS read limit in bytes.
	MaxMessage func() int64
	// Debug enables verbose connection-lifecycle logging (COPYSYNC_DEBUG=1),
	// read once at startup. Lines are prefixed "ws-debug" so they are greppable.
	Debug bool
}

// dbg emits a greppable connection-lifecycle line when debug is enabled.
func (d Deps) dbg(event string, args ...any) {
	if !d.Debug {
		return
	}
	d.Log.Info("ws-debug "+event, args...)
}

// Handler returns the http.Handler for GET /ws.
func Handler(d Deps) http.HandlerFunc {
	if d.Now == nil {
		d.Now = time.Now
	}
	return func(w http.ResponseWriter, r *http.Request) {
		c, err := websocket.Accept(w, r, &websocket.AcceptOptions{
			// Native clients send no Origin header; the admin SPA never uses /ws
			// (it speaks REST only), so origin checking is intentionally skipped.
			InsecureSkipVerify: true,
		})
		if err != nil {
			d.dbg("accept-fail", "remote", r.RemoteAddr, "err", err)
			return
		}
		d.dbg("accept", "remote", r.RemoteAddr)
		serve(r.Context(), d, c, r.RemoteAddr)
	}
}

func serve(parent context.Context, d Deps, c *websocket.Conn, remote string) {
	defer c.CloseNow()
	c.SetReadLimit(d.MaxMessage())

	// 1) Read the hello frame within a deadline.
	helloCtx, cancel := context.WithTimeout(parent, helloTimeout)
	_, data, err := c.Read(helloCtx)
	cancel()
	if err != nil {
		d.dbg("hello-read-fail", "remote", remote, "close_code", websocket.CloseStatus(err), "err", err)
		return
	}
	env, err := protocol.DecodeEnvelope(data)
	if err != nil || env.T != protocol.TypeHello {
		d.dbg("hello-reject", "remote", remote, "reason", "expected a hello frame")
		writeHelloErr(parent, c, "bad_hello", "expected a hello frame")
		return
	}
	var hello protocol.Hello
	if err := env.Decode(&hello); err != nil {
		d.dbg("hello-reject", "remote", remote, "reason", "malformed hello", "err", err)
		writeHelloErr(parent, c, "bad_hello", "malformed hello")
		return
	}
	dev, ok := d.ValidateToken(hello.DeviceID, hello.Token)
	if !ok || dev.Revoked {
		d.dbg("hello-reject", "remote", remote, "device", hello.DeviceID, "reason", "unauthorized")
		writeHelloErr(parent, c, "unauthorized", "invalid device or token")
		return
	}
	d.dbg("handshake-ok", "remote", remote, "device", dev.ID, "name", dev.Name)

	// 2) Start the write pump, then register (which enqueues HelloOK + queued clips).
	client := hub.NewClient(dev, sendBuffer)
	ctx, cancelAll := context.WithCancel(parent)
	defer cancelAll()

	go writePump(ctx, d, c, client, dev.ID)
	d.Hub.Register(client)
	defer d.Hub.Unregister(client)

	// 2b) Token rotation: if the server re-issued this device's token, deliver it
	// so the client persists it (the old token is retired once the new is used).
	if d.MaybeRotateToken != nil {
		if newTok := d.MaybeRotateToken(hello.DeviceID, hello.Token); newTok != "" {
			if b, err := protocol.Encode(protocol.TypeTokenRotate, protocol.TokenRotate{Token: newTok}); err == nil {
				client.Enqueue(b)
			}
		}
	}

	// 3) Read pump: blocks until the connection ends.
	readPump(ctx, c, client, d, hello.DeviceID, remote)
}

func writePump(ctx context.Context, d Deps, c *websocket.Conn, client *hub.Client, device model.DeviceID) {
	ping := time.NewTicker(pingInterval)
	defer ping.Stop()
	for {
		select {
		case <-ctx.Done():
			d.dbg("disconnect", "device", device, "side", "server", "reason", "context cancelled", "pump", "write")
			return
		case <-client.Done():
			// Intentional server-side eviction/replacement. Use StatusGoingAway
			// (1001) so clients treat it as an expected close rather than a
			// protocol error (it was StatusPolicyViolation / 1008 before).
			code := websocket.StatusGoingAway
			d.dbg("disconnect", "device", device, "side", "server", "reason", client.CloseReason(), "evicted", client.Evicted(), "close_code", code, "pump", "write")
			_ = c.Close(code, truncateReason(client.CloseReason()))
			return
		case b := <-client.Send:
			wctx, cancel := context.WithTimeout(ctx, writeTimeout)
			start := d.Now()
			err := c.Write(wctx, websocket.MessageText, b)
			cancel()
			if err != nil {
				d.dbg("disconnect", "device", device, "side", "local", "reason", "write error", "close_code", websocket.CloseStatus(err), "err", err, "pump", "write")
				return
			}
			if took := d.Now().Sub(start); took >= slowWriteThreshold {
				d.dbg("slow-write", "device", device, "took_ms", took.Milliseconds(), "deadline_ms", writeTimeout.Milliseconds(), "bytes", len(b))
			}
		case <-ping.C:
			pctx, cancel := context.WithTimeout(ctx, writeTimeout)
			start := d.Now()
			err := c.Ping(pctx)
			cancel()
			if err != nil {
				d.dbg("disconnect", "device", device, "side", "local", "reason", "ping failed", "close_code", websocket.CloseStatus(err), "err", err, "pump", "write")
				return
			}
			if took := d.Now().Sub(start); took >= slowWriteThreshold {
				d.dbg("slow-ping", "device", device, "took_ms", took.Milliseconds(), "deadline_ms", writeTimeout.Milliseconds())
			}
		}
	}
}

func readPump(ctx context.Context, c *websocket.Conn, client *hub.Client, d Deps, originID model.DeviceID, remote string) {
	for {
		// Bound each read by an idle deadline so a half-open (dead) peer is reaped
		// within idleTimeout instead of lingering until the next failed write.
		// The deadline is reset for every received frame because it is recreated
		// each loop iteration.
		rctx, cancel := context.WithTimeout(ctx, idleTimeout)
		_, data, err := c.Read(rctx)
		cancel()
		if err != nil {
			switch {
			case ctx.Err() != nil:
				// Parent context ended (server shutdown or eviction unwinding).
				d.dbg("disconnect", "device", originID, "remote", remote, "side", "server", "reason", "context cancelled", "pump", "read")
			case rctx.Err() != nil:
				// Our idle deadline fired but the parent is still live: the peer
				// went silent (half-open). Reap it.
				d.dbg("disconnect", "device", originID, "remote", remote, "side", "server", "reason", "idle read timeout (half-open peer)", "idle_s", int(idleTimeout.Seconds()), "pump", "read")
			default:
				// Normal close or read error initiated by the peer.
				d.dbg("disconnect", "device", originID, "remote", remote, "side", "remote", "reason", "read ended", "close_code", websocket.CloseStatus(err), "err", err, "pump", "read")
			}
			return
		}
		env, err := protocol.DecodeEnvelope(data)
		if err != nil {
			continue
		}
		switch env.T {
		case protocol.TypeClip:
			var ev model.ClipEvent
			if err := env.Decode(&ev); err != nil {
				d.Log.Warn("dropping undecodable clip frame", "device", originID, "err", err)
				continue
			}
			// Trust the authenticated identity, not the client's self-report.
			ev.OriginDevice = originID
			if ev.TS == "" {
				ev.TS = d.Now().Format(time.RFC3339)
			}
			res := d.Hub.Route(ev)
			ack := protocol.Ack{ID: ev.ID, Status: res.Status, QueuedFor: res.QueuedFor}
			if b, err := protocol.Encode(protocol.TypeAck, ack); err == nil {
				client.Enqueue(b)
			}
		case protocol.TypeSetPool:
			var sp protocol.SetPool
			if err := env.Decode(&sp); err == nil {
				d.Hub.SetPool(originID, sp.Pool)
			}
		default:
			// Ignore unknown frame types for forward compatibility.
		}
	}
}

func writeHelloErr(ctx context.Context, c *websocket.Conn, code, msg string) {
	if b, err := protocol.Encode(protocol.TypeHelloErr, protocol.HelloErr{Code: code, Message: msg}); err == nil {
		wctx, cancel := context.WithTimeout(ctx, writeTimeout)
		_ = c.Write(wctx, websocket.MessageText, b)
		cancel()
	}
	_ = c.Close(websocket.StatusPolicyViolation, code)
}

func truncateReason(s string) string {
	if len(s) > 120 {
		return s[:120]
	}
	return s
}
