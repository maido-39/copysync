package main

import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"mime"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/coder/websocket"
	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/protocol"
)

// pinnedTLS builds a tls.Config that accepts exactly the certificate whose
// SubjectPublicKeyInfo SHA-256 matches pinB64 (the SPKI pin), and nothing else.
func pinnedTLS(pinB64 string) (*tls.Config, error) {
	pin, err := base64.StdEncoding.DecodeString(pinB64)
	if err != nil || len(pin) != sha256.Size {
		return nil, errors.New("invalid SPKI pin")
	}
	return &tls.Config{
		// We deliberately replace the default verification with our own SPKI
		// check below — trust is anchored on the pin, not on a CA.
		InsecureSkipVerify: true,
		VerifyPeerCertificate: func(rawCerts [][]byte, _ [][]*x509.Certificate) error {
			if len(rawCerts) == 0 {
				return errors.New("server presented no certificate")
			}
			cert, err := x509.ParseCertificate(rawCerts[0])
			if err != nil {
				return err
			}
			sum := sha256.Sum256(cert.RawSubjectPublicKeyInfo)
			if !hmac.Equal(sum[:], pin) {
				return errors.New("server SPKI pin mismatch (possible MITM)")
			}
			return nil
		},
	}, nil
}

func pinnedHTTPClient(pinB64 string) (*http.Client, error) {
	tc, err := pinnedTLS(pinB64)
	if err != nil {
		return nil, err
	}
	return &http.Client{Timeout: 60 * time.Second, Transport: &http.Transport{TLSClientConfig: tc}}, nil
}

type serverInfo struct {
	ServerID   string `json:"serverId"`
	ServerName string `json:"serverName"`
	SPKIPin    string `json:"spkiPin"`
	Proto      int    `json:"proto"`
}

// fetchServerInfoInsecure reads /pair/serverinfo WITHOUT pinning, for
// trust-on-first-use discovery of the pin. Only used at pairing time.
func fetchServerInfoInsecure(serverURL string) (serverInfo, error) {
	c := &http.Client{Timeout: 15 * time.Second, Transport: &http.Transport{TLSClientConfig: &tls.Config{InsecureSkipVerify: true}}}
	var si serverInfo
	resp, err := c.Get(strings.TrimRight(serverURL, "/") + "/pair/serverinfo")
	if err != nil {
		return si, err
	}
	defer resp.Body.Close()
	return si, json.NewDecoder(resp.Body).Decode(&si)
}

// claimPairing redeems an OTP over the pinned connection and returns a Config.
func claimPairing(httpc *http.Client, serverURL, otp, name string) (Config, error) {
	body, _ := json.Marshal(map[string]string{"otp": otp, "deviceName": name, "platform": "linux"})
	resp, err := httpc.Post(strings.TrimRight(serverURL, "/")+"/pair/claim", "application/json", bytes.NewReader(body))
	if err != nil {
		return Config{}, err
	}
	defer resp.Body.Close()
	data, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		return Config{}, fmt.Errorf("pair failed (HTTP %d): %s", resp.StatusCode, strings.TrimSpace(string(data)))
	}
	var r struct {
		DeviceID   string `json:"deviceId"`
		Token      string `json:"token"`
		ServerID   string `json:"serverId"`
		ServerName string `json:"serverName"`
	}
	if err := json.Unmarshal(data, &r); err != nil {
		return Config{}, err
	}
	return Config{ServerURL: serverURL, ServerName: r.ServerName, DeviceID: r.DeviceID, DeviceName: name, Token: r.Token}, nil
}

// Client is a paired copyctl device.
type Client struct {
	cfg   Config
	httpc *http.Client
	echo  *echoGuard
	hist  *History

	mu     sync.Mutex
	seq    uint64
	served map[model.BlobID]string // on-demand blobs this client holds: id -> file path
}

func newClient(cfg Config, hist *History) (*Client, error) {
	hc, err := pinnedHTTPClient(cfg.Pin)
	if err != nil {
		return nil, err
	}
	return &Client{cfg: cfg, httpc: hc, echo: newEchoGuard(), hist: hist, served: make(map[model.BlobID]string)}, nil
}

func (c *Client) wsURL() string {
	u := strings.TrimRight(c.cfg.ServerURL, "/")
	u = strings.Replace(u, "https://", "wss://", 1)
	u = strings.Replace(u, "http://", "ws://", 1)
	return u + "/ws"
}

func (c *Client) connect(ctx context.Context) (*websocket.Conn, protocol.HelloOK, error) {
	conn, _, err := websocket.Dial(ctx, c.wsURL(), &websocket.DialOptions{HTTPClient: c.httpc})
	if err != nil {
		return nil, protocol.HelloOK{}, err
	}
	conn.SetReadLimit(16 << 20)
	hello := protocol.Hello{
		DeviceID: model.DeviceID(c.cfg.DeviceID), DeviceName: c.cfg.DeviceName,
		Token: c.cfg.Token, Platform: "linux", Proto: protocol.Proto,
	}
	if err := writeMsg(ctx, conn, protocol.TypeHello, hello); err != nil {
		_ = conn.CloseNow()
		return nil, protocol.HelloOK{}, err
	}
	env, err := readMsg(ctx, conn)
	if err != nil {
		_ = conn.CloseNow()
		return nil, protocol.HelloOK{}, err
	}
	switch env.T {
	case protocol.TypeHelloOK:
		var ok protocol.HelloOK
		_ = env.Decode(&ok)
		return conn, ok, nil
	case protocol.TypeHelloErr:
		var he protocol.HelloErr
		_ = env.Decode(&he)
		_ = conn.CloseNow()
		return nil, protocol.HelloOK{}, fmt.Errorf("server rejected connection: %s (%s)", he.Message, he.Code)
	default:
		_ = conn.CloseNow()
		return nil, protocol.HelloOK{}, fmt.Errorf("unexpected first frame %q", env.T)
	}
}

func (c *Client) nextSeq() uint64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.seq++
	return c.seq
}

func (c *Client) sendText(ctx context.Context, conn *websocket.Conn, text string, targets model.Targets) (string, error) {
	sum := sha256.Sum256([]byte(text))
	ev := model.ClipEvent{
		ID: newID(), Seq: c.nextSeq(), TS: time.Now().Format(time.RFC3339), Mime: []string{"text/plain"},
		InlineText: text, Size: int64(len(text)), Sha256: hex.EncodeToString(sum[:]), Targets: targets,
	}
	return ev.ID, writeMsg(ctx, conn, protocol.TypeClip, ev)
}

func (c *Client) sendBlob(ctx context.Context, conn *websocket.Conn, content []byte, mimeType, name string, targets model.Targets) (string, error) {
	bid, err := c.putBlob(content)
	if err != nil {
		return "", err
	}
	sum := sha256.Sum256(content)
	if mimeType == "" {
		mimeType = "application/octet-stream"
	}
	ev := model.ClipEvent{
		ID: newID(), Seq: c.nextSeq(), TS: time.Now().Format(time.RFC3339), Mime: []string{mimeType},
		Name: name, BlobID: bid, Size: int64(len(content)), Sha256: hex.EncodeToString(sum[:]), Targets: targets,
	}
	return ev.ID, writeMsg(ctx, conn, protocol.TypeClip, ev)
}

func (c *Client) putBlob(content []byte) (model.BlobID, error) {
	sum := sha256.Sum256(content)
	id := "sha256:" + hex.EncodeToString(sum[:])
	req, _ := http.NewRequest(http.MethodPut, strings.TrimRight(c.cfg.ServerURL, "/")+"/blob/"+id, bytes.NewReader(content))
	req.Header.Set("Authorization", "Bearer "+c.cfg.Token)
	req.ContentLength = int64(len(content))
	resp, err := c.httpc.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusCreated && resp.StatusCode != http.StatusOK {
		b, _ := io.ReadAll(resp.Body)
		return "", fmt.Errorf("blob PUT %d: %s", resp.StatusCode, strings.TrimSpace(string(b)))
	}
	return model.BlobID(id), nil
}

func (c *Client) getBlob(id model.BlobID) ([]byte, error) {
	req, _ := http.NewRequest(http.MethodGet, strings.TrimRight(c.cfg.ServerURL, "/")+"/blob/"+string(id), nil)
	req.Header.Set("Authorization", "Bearer "+c.cfg.Token)
	resp, err := c.httpc.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("blob GET %d", resp.StatusCode)
	}
	return io.ReadAll(resp.Body)
}

func (c *Client) waitAck(ctx context.Context, conn *websocket.Conn, id string) (protocol.Ack, error) {
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		env, err := readMsg(ctx, conn)
		if err != nil {
			return protocol.Ack{}, err
		}
		if env.T == protocol.TypeAck {
			var a protocol.Ack
			_ = env.Decode(&a)
			if a.ID == id {
				return a, nil
			}
		}
	}
	return protocol.Ack{}, errors.New("timed out waiting for ack")
}

func writeMsg(ctx context.Context, conn *websocket.Conn, t string, payload any) error {
	b, err := protocol.Encode(t, payload)
	if err != nil {
		return err
	}
	return conn.Write(ctx, websocket.MessageText, b)
}

func readMsg(ctx context.Context, conn *websocket.Conn) (protocol.Envelope, error) {
	_, data, err := conn.Read(ctx)
	if err != nil {
		return protocol.Envelope{}, err
	}
	return protocol.DecodeEnvelope(data)
}

func newID() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	return hex.EncodeToString(b[:])
}

func parseTargets(s string) model.Targets {
	s = strings.TrimSpace(s)
	if s == "" || s == "all" {
		return model.Targets{All: true}
	}
	var ids []model.DeviceID
	for _, p := range strings.Split(s, ",") {
		if p = strings.TrimSpace(p); p != "" {
			ids = append(ids, model.DeviceID(p))
		}
	}
	return model.Targets{Devices: ids}
}

// echoGuard records the hashes of clips we just wrote to the OS clipboard so we
// don't rebroadcast them when the clipboard-change event fires.
type echoGuard struct {
	mu     sync.Mutex
	seenAt map[string]time.Time
}

func newEchoGuard() *echoGuard { return &echoGuard{seenAt: make(map[string]time.Time)} }

func (e *echoGuard) markWritten(sha string) {
	e.mu.Lock()
	e.seenAt[sha] = time.Now()
	e.mu.Unlock()
}

func (e *echoGuard) seen(sha string) bool {
	e.mu.Lock()
	defer e.mu.Unlock()
	t, ok := e.seenAt[sha]
	return ok && time.Since(t) < 10*time.Second
}

func (c *Client) holdFile(id model.BlobID, path string) {
	c.mu.Lock()
	c.served[id] = path
	c.mu.Unlock()
}

func (c *Client) servedPath(id model.BlobID) (string, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	p, ok := c.served[id]
	return p, ok
}

func fileSHA256(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}

func mimeOf(path string) string {
	if t := mime.TypeByExtension(filepath.Ext(path)); t != "" {
		return t
	}
	return "application/octet-stream"
}

// sendLazyClip advertises a file as on-demand (no upload) and remembers it so a
// later blob_request from the server can be served.
func (c *Client) sendLazyClip(ctx context.Context, conn *websocket.Conn, path string, size int64, targets model.Targets) (model.BlobID, error) {
	sum, err := fileSHA256(path)
	if err != nil {
		return "", err
	}
	bid := model.BlobID("sha256:" + sum)
	c.holdFile(bid, path)
	ev := model.ClipEvent{
		ID: newID(), Seq: c.nextSeq(), TS: time.Now().Format(time.RFC3339),
		Mime: []string{mimeOf(path)}, Name: filepath.Base(path), BlobID: bid,
		Size: size, Sha256: sum, OnDemand: true, Targets: targets,
	}
	return bid, writeMsg(ctx, conn, protocol.TypeClip, ev)
}

// serveLoop reads frames and answers blob_request by uploading the held file.
func (c *Client) serveLoop(ctx context.Context, conn *websocket.Conn) error {
	for {
		env, err := readMsg(ctx, conn)
		if err != nil {
			return err
		}
		if env.T != protocol.TypeBlobReq {
			continue
		}
		var br protocol.BlobRequest
		if env.Decode(&br) != nil {
			continue
		}
		path, ok := c.servedPath(model.BlobID(br.ID))
		if !ok {
			continue
		}
		data, err := os.ReadFile(path)
		if err != nil {
			fmt.Fprintln(os.Stderr, "serve: read failed:", err)
			continue
		}
		if _, err := c.putBlob(data); err != nil {
			fmt.Fprintln(os.Stderr, "serve: upload failed:", err)
			continue
		}
		fmt.Printf("served on demand: %s (%d bytes)\n", br.ID, len(data))
	}
}

// pinnedFetch GETs a blob over a long-timeout pinned connection (the server may
// long-poll while it pulls the blob from the origin device on demand).
func (c *Client) pinnedFetch(id model.BlobID, timeout time.Duration) ([]byte, int, error) {
	tc, err := pinnedTLS(c.cfg.Pin)
	if err != nil {
		return nil, 0, err
	}
	hc := &http.Client{Timeout: timeout, Transport: &http.Transport{TLSClientConfig: tc}}
	req, _ := http.NewRequest(http.MethodGet, strings.TrimRight(c.cfg.ServerURL, "/")+"/blob/"+string(id), nil)
	req.Header.Set("Authorization", "Bearer "+c.cfg.Token)
	resp, err := hc.Do(req)
	if err != nil {
		return nil, 0, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		b, _ := io.ReadAll(resp.Body)
		return nil, resp.StatusCode, fmt.Errorf("%s", strings.TrimSpace(string(b)))
	}
	data, err := io.ReadAll(resp.Body)
	return data, resp.StatusCode, err
}
