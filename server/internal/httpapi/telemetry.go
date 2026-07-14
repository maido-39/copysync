package httpapi

import (
	"encoding/json"
	"net/http"
	"strconv"
	"sync"
	"time"
)

// Telemetry lets paired clients upload their system-operation logs (engine
// start/stop, reconnects, errors, pairing) so an operator can diagnose a device
// WITHOUT shell access to it — the local agent.log/engine.log never leave the
// machine otherwise. Storage is a single bounded in-memory ring: it is a
// diagnostic tail, not durable history, and must never itself grow without
// bound (that would just move the leak we are hunting into the server).
//
// Privacy: clients send operational lines only (lifecycle + error text), never
// clipboard content. The ingest endpoint authenticates the device by its bearer
// token (same as the blob channel), so only paired devices can post.

// telemetryCap is the max lines retained server-wide (oldest evicted first).
// ~3000 * a couple hundred bytes ≈ under 1 MB — a bounded tail.
const telemetryCap = 3000

// telemetryMaxBatch bounds a single upload so one client cannot flood the ring.
const telemetryMaxBatch = 500

// telemetryMaxBody caps the ingest request body (defense against a large POST).
const telemetryMaxBody = 512 * 1024

// TelemetryLine is one uploaded log line, tagged with who sent it and when the
// server received it.
type TelemetryLine struct {
	Device   string `json:"device"`       // human device name (or id fallback)
	DeviceID string `json:"deviceId"`     // stable device id
	Client   string `json:"client"`       // "agent" | "gui" | "android" | …
	Level    string `json:"level"`        // "info" | "warn" | "error"
	TS       string `json:"ts,omitempty"` // client-side timestamp (as sent)
	RecvTS   string `json:"recvTs"`       // server receive time (RFC3339)
	Msg      string `json:"msg"`          // the log message
}

// telemetryRing is a thread-safe bounded ring of uploaded log lines.
type telemetryRing struct {
	mu    sync.Mutex
	lines []TelemetryLine
	cap   int
}

func newTelemetryRing(capacity int) *telemetryRing {
	return &telemetryRing{cap: capacity}
}

// add appends lines, evicting the oldest so len never exceeds cap.
func (t *telemetryRing) add(lines []TelemetryLine) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.lines = append(t.lines, lines...)
	if len(t.lines) > t.cap {
		// Keep only the newest cap entries; copy so the backing array of the
		// dropped prefix can be freed (a reslice alone would retain it).
		keep := make([]TelemetryLine, t.cap)
		copy(keep, t.lines[len(t.lines)-t.cap:])
		t.lines = keep
	}
}

// recent returns up to limit newest lines (newest last), optionally filtered by
// device id. A limit <= 0 returns all retained lines.
func (t *telemetryRing) recent(limit int, deviceID string) []TelemetryLine {
	t.mu.Lock()
	defer t.mu.Unlock()
	var src []TelemetryLine
	if deviceID == "" {
		src = t.lines
	} else {
		for _, l := range t.lines {
			if l.DeviceID == deviceID {
				src = append(src, l)
			}
		}
	}
	if limit > 0 && len(src) > limit {
		src = src[len(src)-limit:]
	}
	out := make([]TelemetryLine, len(src))
	copy(out, src)
	return out
}

// telemetryIngest is the request body clients POST to /telemetry.
type telemetryIngest struct {
	Client string            `json:"client"`
	Lines  []telemetryInLine `json:"lines"`
}

type telemetryInLine struct {
	Level string `json:"level"`
	TS    string `json:"ts"`
	Msg   string `json:"msg"`
}

// handleTelemetryIngest accepts a batch of operational log lines from a paired
// device (bearer-token authenticated, like the blob channel) into the bounded
// ring. Over-sized bodies/batches are rejected rather than truncated silently.
func (s *Server) handleTelemetryIngest(w http.ResponseWriter, r *http.Request) {
	dev, ok := s.authBlob(r)
	if !ok {
		writeJSONError(w, http.StatusUnauthorized, "unauthorized", "invalid device token")
		return
	}
	if s.telemetry == nil {
		// Ingestion disabled: ack so clients don't spin on retries.
		writeJSON(w, http.StatusOK, map[string]any{"accepted": 0})
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, telemetryMaxBody)
	var in telemetryIngest
	if err := json.NewDecoder(r.Body).Decode(&in); err != nil {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "invalid telemetry body")
		return
	}
	if len(in.Lines) > telemetryMaxBatch {
		writeJSONError(w, http.StatusRequestEntityTooLarge, "too_large", "too many lines in one batch")
		return
	}
	name := dev.Name
	if name == "" {
		name = string(dev.ID)
	}
	client := sanitizeShort(in.Client, 24)
	if client == "" {
		client = "unknown"
	}
	recv := s.now().UTC().Format(time.RFC3339)
	out := make([]TelemetryLine, 0, len(in.Lines))
	for _, l := range in.Lines {
		msg := sanitizeShort(l.Msg, 4096)
		if msg == "" {
			continue
		}
		out = append(out, TelemetryLine{
			Device:   name,
			DeviceID: string(dev.ID),
			Client:   client,
			Level:    normalizeLevel(l.Level),
			TS:       sanitizeShort(l.TS, 40),
			RecvTS:   recv,
			Msg:      msg,
		})
	}
	s.telemetry.add(out)
	writeJSON(w, http.StatusOK, map[string]any{"accepted": len(out)})
}

// handleAdminTelemetry returns recent telemetry lines for the admin webapp.
// Query: ?limit=N (default 500) & ?device=<id> (optional filter).
func (s *Server) handleAdminTelemetry(w http.ResponseWriter, r *http.Request) {
	if s.telemetry == nil {
		writeJSON(w, http.StatusOK, map[string]any{"lines": []TelemetryLine{}})
		return
	}
	limit := 500
	if q := r.URL.Query().Get("limit"); q != "" {
		if n, err := strconv.Atoi(q); err == nil && n > 0 && n <= telemetryCap {
			limit = n
		}
	}
	lines := s.telemetry.recent(limit, r.URL.Query().Get("device"))
	writeJSON(w, http.StatusOK, map[string]any{"lines": lines})
}

func normalizeLevel(l string) string {
	switch l {
	case "warn", "warning":
		return "warn"
	case "error", "err":
		return "error"
	default:
		return "info"
	}
}

// sanitizeShort trims to n runes and strips control chars (except space) so an
// uploaded line can't break the SSE/JSON view or bloat the ring.
func sanitizeShort(s string, n int) string {
	out := make([]rune, 0, len(s))
	for _, r := range s {
		if r == '\n' || r == '\t' {
			r = ' '
		}
		if r < 0x20 || r == 0x7f {
			continue
		}
		out = append(out, r)
		if len(out) >= n {
			break
		}
	}
	return string(out)
}
