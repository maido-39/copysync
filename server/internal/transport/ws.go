// Package transport implements the WebSocket control channel: it upgrades the
// connection, performs the hello/token handshake, and bridges the socket to the
// hub via a read pump and a write pump.
package transport

import (
	"context"
	"log/slog"
	"net/http"
	"sync/atomic"
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
	// idleTimeout bounds how long the connection may go with NO sign of life from
	// the peer before it is reaped as half-open. Liveness is tracked by a shared
	// lastActivity timestamp that is bumped on (i) every inbound frame and (ii)
	// every successful server->client Ping (coder/websocket's Ping blocks until
	// the peer's pong arrives, so a successful Ping proves the peer is alive even
	// when it is sending no data frames). A separate watchdog ticker tears the
	// connection down only when now-lastActivity exceeds idleTimeout. This must
	// NOT be enforced as a per-read deadline: coder/websocket consumes ping/pong
	// control frames inside its read loop without surfacing them as reads, so a
	// healthy-but-idle peer that only answers pings would otherwise be reaped.
	idleTimeout = 75 * time.Second
	// watchdogInterval is how often the liveness watchdog checks lastActivity.
	watchdogInterval = 15 * time.Second
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
	// MaybeRotateToken runs after a successful auth (Stage-3 token rotation). It
	// retires a now-confirmed previous token and decides whether to re-issue the
	// bearer token, returning a RotateDecision whose Token is the NEW plaintext
	// token to deliver (or "" for none). The decision is self-contained: it also
	// carries whether this is a grace re-issue (and the grace hash) so the commit
	// step needs no cross-connection side state. It MUST NOT durably commit the
	// rotation: the caller only commits once the new token has actually been
	// queued for delivery (via CommitTokenRotation), so a dropped token_rotate
	// frame cannot wedge the device on a never-retired token the client never
	// learns.
	MaybeRotateToken func(model.DeviceID, string) RotateDecision
	// CommitTokenRotation durably records a rotation issued by MaybeRotateToken,
	// keyed by the same decision. Called only after the token_rotate frame was
	// successfully enqueued.
	CommitTokenRotation func(model.DeviceID, RotateDecision)
	// MaxMessage returns the current WS read limit in bytes.
	MaxMessage func() int64
	// Debug enables verbose connection-lifecycle logging (COPYSYNC_DEBUG=1),
	// read once at startup. Lines are prefixed "ws-debug" so they are greppable.
	Debug bool
}

// RotateDecision is the self-contained outcome of MaybeRotateToken. Token is the
// new plaintext token to deliver (empty means "no rotation"). When Reissue is
// true the rotation re-mints an orphaned, never-delivered token from the grace
// state, and PrevHash is the grace (previous) token hash the device presented;
// the commit step uses these instead of any cross-connection map, so a dropped
// token_rotate frame can never leak an intent that wedges a later rotation.
type RotateDecision struct {
	Token    string
	Reissue  bool
	PrevHash string
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

	// Shared liveness clock (UnixNano). Bumped by readPump on every inbound frame
	// and by writePump on every successful ping; watched by the idle watchdog.
	var lastActivity atomic.Int64
	lastActivity.Store(d.Now().UnixNano())

	go writePump(ctx, d, c, client, dev.ID, &lastActivity)
	go idleWatchdog(ctx, d, c, dev.ID, remote, &lastActivity)
	d.Hub.Register(client)
	defer d.Hub.Unregister(client)

	// 2b) Token rotation: if the server re-issued this device's token, deliver it
	// so the client persists it (the old token is retired once the new is used).
	// The rotation is committed to the store ONLY after the frame is actually
	// queued; if the send buffer is full and the frame would be dropped, we skip
	// the commit so the device keeps its current token and rotation is retried on
	// the next reconnect — rather than wedging on a new token it never learned.
	if d.MaybeRotateToken != nil {
		if dec := d.MaybeRotateToken(hello.DeviceID, hello.Token); dec.Token != "" {
			if b, err := protocol.Encode(protocol.TypeTokenRotate, protocol.TokenRotate{Token: dec.Token}); err == nil {
				if client.Enqueue(b) {
					if d.CommitTokenRotation != nil {
						d.CommitTokenRotation(hello.DeviceID, dec)
					}
				} else {
					d.Log.Warn("token_rotate frame dropped (send buffer full); rotation deferred to next reconnect", "device", hello.DeviceID)
				}
			}
		}
	}

	// 3) Read pump: blocks until the connection ends.
	readPump(ctx, c, client, d, hello.DeviceID, remote, &lastActivity)
}

func writePump(ctx context.Context, d Deps, c *websocket.Conn, client *hub.Client, device model.DeviceID, lastActivity *atomic.Int64) {
	// NOTE: pinging is deliberately NOT done here. coder/websocket's Ping blocks
	// until the peer's pong arrives (up to writeTimeout); doing it in this select
	// would stall draining client.Send for that whole window and could let a burst
	// fill the send buffer and trigger a spurious "send buffer full" eviction of an
	// otherwise-healthy client. Liveness pinging lives in idleWatchdog instead, so
	// this goroutine only ever blocks on actual data writes.
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
		}
	}
}

func readPump(ctx context.Context, c *websocket.Conn, client *hub.Client, d Deps, originID model.DeviceID, remote string, lastActivity *atomic.Int64) {
	for {
		// Read on the parent context (no per-read idle deadline): coder/websocket
		// consumes ping/pong control frames inside its read loop without surfacing
		// them as reads, so bounding the read by idleTimeout would reap a healthy
		// peer that only answers pings. Half-open detection is instead handled by
		// idleWatchdog, which closes the conn when neither a frame nor a ping-pong
		// has been seen within idleTimeout.
		_, data, err := c.Read(ctx)
		if err != nil {
			if ctx.Err() != nil {
				// Parent context ended (server shutdown, eviction, or watchdog reap).
				d.dbg("disconnect", "device", originID, "remote", remote, "side", "server", "reason", "context cancelled", "pump", "read")
			} else {
				// Normal close or read error initiated by the peer.
				d.dbg("disconnect", "device", originID, "remote", remote, "side", "remote", "reason", "read ended", "close_code", websocket.CloseStatus(err), "err", err, "pump", "read")
			}
			return
		}
		// Any inbound frame is a sign of life.
		lastActivity.Store(d.Now().UnixNano())
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

// idleWatchdog owns liveness: it pings the peer on its own goroutine and reaps a
// half-open peer. Pinging lives here (not in writePump) so a Ping's pong-wait
// never stalls the data-write path. coder/websocket's Ping may run concurrently
// with the Reader/Writer — pongs are consumed by readPump — so issuing it from
// this goroutine is safe. A successful ping bumps lastActivity (proving a
// data-silent peer is alive); if neither an inbound frame nor a ping-pong has
// updated lastActivity within idleTimeout, the connection is closed (which
// unblocks readPump's c.Read).
func idleWatchdog(ctx context.Context, d Deps, c *websocket.Conn, device model.DeviceID, remote string, lastActivity *atomic.Int64) {
	ping := time.NewTicker(pingInterval)
	defer ping.Stop()
	watch := time.NewTicker(watchdogInterval)
	defer watch.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ping.C:
			pctx, cancel := context.WithTimeout(ctx, writeTimeout)
			start := d.Now()
			err := c.Ping(pctx)
			cancel()
			if err != nil {
				// Ping failed/timed out: the peer is gone. Closing unblocks readPump.
				d.dbg("disconnect", "device", device, "remote", remote, "side", "local", "reason", "ping failed", "close_code", websocket.CloseStatus(err), "err", err, "pump", "watchdog")
				_ = c.Close(websocket.StatusGoingAway, "ping failed")
				return
			}
			// A successful Ping blocks until the peer's pong arrives, so it proves
			// the peer is alive even when it sends no data frames. Bump liveness so
			// the watchdog never reaps a healthy-but-idle client.
			lastActivity.Store(d.Now().UnixNano())
			if took := d.Now().Sub(start); took >= slowWriteThreshold {
				d.dbg("slow-ping", "device", device, "took_ms", took.Milliseconds(), "deadline_ms", writeTimeout.Milliseconds())
			}
		case <-watch.C:
			idle := d.Now().Sub(time.Unix(0, lastActivity.Load()))
			if idle >= idleTimeout {
				d.dbg("disconnect", "device", device, "remote", remote, "side", "server", "reason", "idle timeout (half-open peer)", "idle_s", int(idle.Seconds()), "pump", "watchdog")
				_ = c.Close(websocket.StatusGoingAway, "idle timeout")
				return
			}
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
