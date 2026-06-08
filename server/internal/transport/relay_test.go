package transport_test

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/syaro/copysync/internal/auth"
	"github.com/syaro/copysync/internal/hub"
	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/protocol"
	"github.com/syaro/copysync/internal/store"
	"github.com/syaro/copysync/internal/transport"
)

const testSecret = "integration-test-secret"

func newTestServer(t *testing.T) (*httptest.Server, *store.Store) {
	t.Helper()
	st, err := store.Open(t.TempDir())
	if err != nil {
		t.Fatalf("open store: %v", err)
	}
	t.Cleanup(func() { _ = st.Close() })

	log := slog.New(slog.NewTextHandler(io.Discard, nil))
	h := hub.New(st, log, time.Now, "srv_test", "Test")
	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	go h.Run(ctx)

	validate := func(id model.DeviceID, token string) (model.Device, bool) {
		dev, found, _ := st.GetDevice(id)
		if !found || dev.Revoked {
			return model.Device{}, false
		}
		rec, found, _ := st.GetToken(id)
		if !found {
			return model.Device{}, false
		}
		if !auth.ConstantTimeEqual(rec.TokenHash, auth.HashToken(testSecret, token)) {
			return model.Device{}, false
		}
		return dev, true
	}

	mux := http.NewServeMux()
	mux.Handle("GET /ws", transport.Handler(transport.Deps{
		Hub: h, Log: log, Now: time.Now, ValidateToken: validate,
		MaxMessage: func() int64 { return 1 << 20 },
	}))
	ts := httptest.NewTLSServer(mux)
	t.Cleanup(ts.Close)
	return ts, st
}

func pairDevice(t *testing.T, st *store.Store, name string) (model.DeviceID, string) {
	t.Helper()
	id := model.DeviceID(auth.NewID("dev"))
	token := auth.GenerateToken()
	now := time.Now()
	if err := st.PutDevice(model.Device{ID: id, Name: name, Platform: "test", CreatedAt: now, LastSeenAt: now}); err != nil {
		t.Fatal(err)
	}
	if err := st.PutToken(model.TokenRecord{DeviceID: id, TokenHash: auth.HashToken(testSecret, token), IssuedAt: now}); err != nil {
		t.Fatal(err)
	}
	return id, token
}

func dialWS(t *testing.T, ts *httptest.Server) *websocket.Conn {
	t.Helper()
	url := "wss" + strings.TrimPrefix(ts.URL, "https") + "/ws"
	c, _, err := websocket.Dial(context.Background(), url, &websocket.DialOptions{HTTPClient: ts.Client()})
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	t.Cleanup(func() { _ = c.CloseNow() })
	return c
}

func send(t *testing.T, c *websocket.Conn, typ string, payload any) {
	t.Helper()
	b, err := protocol.Encode(typ, payload)
	if err != nil {
		t.Fatal(err)
	}
	if err := c.Write(context.Background(), websocket.MessageText, b); err != nil {
		t.Fatalf("write: %v", err)
	}
}

func doHello(t *testing.T, c *websocket.Conn, id model.DeviceID, name, token string) protocol.HelloOK {
	t.Helper()
	send(t, c, protocol.TypeHello, protocol.Hello{DeviceID: id, DeviceName: name, Token: token, Platform: "test", Proto: 1})
	env := waitFor(t, c, protocol.TypeHelloOK)
	var ok protocol.HelloOK
	if err := env.Decode(&ok); err != nil {
		t.Fatal(err)
	}
	return ok
}

// waitFor reads frames until one of the wanted type arrives, skipping others
// (e.g. presence/roster), or fails after a deadline.
func waitFor(t *testing.T, c *websocket.Conn, typ string) protocol.Envelope {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		_, data, err := c.Read(ctx)
		cancel()
		if err != nil {
			t.Fatalf("read while waiting for %s: %v", typ, err)
		}
		env, err := protocol.DecodeEnvelope(data)
		if err != nil {
			continue
		}
		if env.T == typ {
			return env
		}
	}
	t.Fatalf("timed out waiting for %s frame", typ)
	return protocol.Envelope{}
}

func expectNoClip(t *testing.T, c *websocket.Conn) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 600*time.Millisecond)
	defer cancel()
	_, data, err := c.Read(ctx)
	if err != nil {
		return // timeout: nothing arrived, as expected
	}
	if env, err := protocol.DecodeEnvelope(data); err == nil && env.T == protocol.TypeClip {
		t.Fatalf("origin unexpectedly received its own clip")
	}
}

func TestBadTokenRejected(t *testing.T) {
	ts, st := newTestServer(t)
	id, _ := pairDevice(t, st, "A")
	c := dialWS(t, ts)
	send(t, c, protocol.TypeHello, protocol.Hello{DeviceID: id, Token: "wrong-token", Proto: 1})
	env := waitFor(t, c, protocol.TypeHelloErr)
	var he protocol.HelloErr
	_ = env.Decode(&he)
	if he.Code != "unauthorized" {
		t.Fatalf("expected unauthorized, got %q", he.Code)
	}
}

func TestRelayTextClip(t *testing.T) {
	ts, st := newTestServer(t)
	idA, tokA := pairDevice(t, st, "A")
	idB, tokB := pairDevice(t, st, "B")

	ca := dialWS(t, ts)
	cb := dialWS(t, ts)
	okA := doHello(t, ca, idA, "A", tokA)
	doHello(t, cb, idB, "B", tokB)

	if okA.ServerName != "Test" || okA.MaxMsg == 0 {
		t.Fatalf("unexpected hello_ok: %+v", okA)
	}

	send(t, ca, protocol.TypeClip, model.ClipEvent{
		ID: "clip1", Seq: 1, Mime: []string{"text/plain"},
		InlineText: "hello world", Sha256: "abc", Targets: model.Targets{All: true},
	})

	// A gets an ack...
	var ack protocol.Ack
	_ = waitFor(t, ca, protocol.TypeAck).Decode(&ack)
	if ack.Status != protocol.AckRelayed {
		t.Fatalf("ack status = %q, want relayed", ack.Status)
	}
	// ...and never receives its own clip back (echo suppression).
	expectNoClip(t, ca)

	// B receives the clip with the authenticated origin.
	var got model.ClipEvent
	_ = waitFor(t, cb, protocol.TypeClip).Decode(&got)
	if got.InlineText != "hello world" {
		t.Fatalf("B got text %q", got.InlineText)
	}
	if got.OriginDevice != idA {
		t.Fatalf("origin = %s, want %s", got.OriginDevice, idA)
	}
}

func TestOfflineQueueDrain(t *testing.T) {
	ts, st := newTestServer(t)
	idA, tokA := pairDevice(t, st, "A")
	idC, tokC := pairDevice(t, st, "C")

	ca := dialWS(t, ts)
	doHello(t, ca, idA, "A", tokA)

	// C is offline, so a broadcast is queued for it.
	send(t, ca, protocol.TypeClip, model.ClipEvent{
		ID: "q1", Seq: 1, Mime: []string{"text/plain"},
		InlineText: "queued message", Sha256: "h1", Targets: model.Targets{All: true},
	})
	var ack protocol.Ack
	_ = waitFor(t, ca, protocol.TypeAck).Decode(&ack)
	if ack.Status != protocol.AckQueued {
		t.Fatalf("ack status = %q, want queued (queuedFor=%v)", ack.Status, ack.QueuedFor)
	}
	if n, _ := st.QueueLen(idC); n != 1 {
		t.Fatalf("queue len for C = %d, want 1", n)
	}

	// C connects and drains its queue.
	cc := dialWS(t, ts)
	doHello(t, cc, idC, "C", tokC)
	var got model.ClipEvent
	_ = waitFor(t, cc, protocol.TypeClip).Decode(&got)
	if got.InlineText != "queued message" {
		t.Fatalf("C drained text %q", got.InlineText)
	}
	if n, _ := st.QueueLen(idC); n != 0 {
		t.Fatalf("queue len after drain = %d, want 0", n)
	}
}

func TestTargetedRoutingSkipsOthers(t *testing.T) {
	ts, st := newTestServer(t)
	idA, tokA := pairDevice(t, st, "A")
	idB, tokB := pairDevice(t, st, "B")
	idC, tokC := pairDevice(t, st, "C")

	ca := dialWS(t, ts)
	cb := dialWS(t, ts)
	cc := dialWS(t, ts)
	doHello(t, ca, idA, "A", tokA)
	doHello(t, cb, idB, "B", tokB)
	doHello(t, cc, idC, "C", tokC)

	// A targets only C.
	send(t, ca, protocol.TypeClip, model.ClipEvent{
		ID: "t1", Seq: 1, Mime: []string{"text/plain"}, InlineText: "for C only",
		Sha256: "hc", Targets: model.Targets{Devices: []model.DeviceID{idC}},
	})

	var got model.ClipEvent
	_ = waitFor(t, cc, protocol.TypeClip).Decode(&got)
	if got.InlineText != "for C only" {
		t.Fatalf("C got %q", got.InlineText)
	}
	// B must not receive it.
	expectNoClip(t, cb)
}
