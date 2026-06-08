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
)

// Deps are the dependencies of the WS handler.
type Deps struct {
	Hub *hub.Hub
	Log *slog.Logger
	Now func() time.Time
	// ValidateToken returns the device for a (deviceID, token) pair, or false.
	ValidateToken func(model.DeviceID, string) (model.Device, bool)
	// MaxMessage returns the current WS read limit in bytes.
	MaxMessage func() int64
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
			return
		}
		serve(r.Context(), d, c)
	}
}

func serve(parent context.Context, d Deps, c *websocket.Conn) {
	defer c.CloseNow()
	c.SetReadLimit(d.MaxMessage())

	// 1) Read the hello frame within a deadline.
	helloCtx, cancel := context.WithTimeout(parent, helloTimeout)
	_, data, err := c.Read(helloCtx)
	cancel()
	if err != nil {
		return
	}
	env, err := protocol.DecodeEnvelope(data)
	if err != nil || env.T != protocol.TypeHello {
		writeHelloErr(parent, c, "bad_hello", "expected a hello frame")
		return
	}
	var hello protocol.Hello
	if err := env.Decode(&hello); err != nil {
		writeHelloErr(parent, c, "bad_hello", "malformed hello")
		return
	}
	dev, ok := d.ValidateToken(hello.DeviceID, hello.Token)
	if !ok || dev.Revoked {
		writeHelloErr(parent, c, "unauthorized", "invalid device or token")
		return
	}

	// 2) Start the write pump, then register (which enqueues HelloOK + queued clips).
	client := hub.NewClient(dev, sendBuffer)
	ctx, cancelAll := context.WithCancel(parent)
	defer cancelAll()

	go writePump(ctx, c, client)
	d.Hub.Register(client)
	defer d.Hub.Unregister(client)

	// 3) Read pump: blocks until the connection ends.
	readPump(ctx, c, client, d, hello.DeviceID)
}

func writePump(ctx context.Context, c *websocket.Conn, client *hub.Client) {
	ping := time.NewTicker(pingInterval)
	defer ping.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-client.Done():
			_ = c.Close(websocket.StatusPolicyViolation, truncateReason(client.CloseReason()))
			return
		case b := <-client.Send:
			wctx, cancel := context.WithTimeout(ctx, writeTimeout)
			err := c.Write(wctx, websocket.MessageText, b)
			cancel()
			if err != nil {
				return
			}
		case <-ping.C:
			pctx, cancel := context.WithTimeout(ctx, writeTimeout)
			err := c.Ping(pctx)
			cancel()
			if err != nil {
				return
			}
		}
	}
}

func readPump(ctx context.Context, c *websocket.Conn, client *hub.Client, d Deps, originID model.DeviceID) {
	for {
		_, data, err := c.Read(ctx)
		if err != nil {
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
				continue
			}
			// Trust the authenticated identity, not the client's self-report.
			ev.OriginDevice = originID
			if ev.TS.IsZero() {
				ev.TS = d.Now()
			}
			res := d.Hub.Route(ev)
			ack := protocol.Ack{ID: ev.ID, Status: res.Status, QueuedFor: res.QueuedFor}
			if b, err := protocol.Encode(protocol.TypeAck, ack); err == nil {
				client.Enqueue(b)
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
