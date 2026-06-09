package httpapi

import (
	"encoding/json"
	"fmt"
	"net/http"

	"github.com/syaro/copysync/internal/hub"
)

// handleMonitorStream is a Server-Sent Events stream of relayed clips for the
// admin Monitoring tab. Non-E2E clips include an inline-text preview; E2E clips
// show only a ciphertext marker (the server cannot read them).
func (s *Server) handleMonitorStream(w http.ResponseWriter, r *http.Request) {
	fl, ok := w.(http.Flusher)
	if !ok {
		writeJSONError(w, http.StatusInternalServerError, "unsupported", "streaming unsupported")
		return
	}
	h := w.Header()
	h.Set("Content-Type", "text/event-stream")
	h.Set("Cache-Control", "no-cache")
	h.Set("Connection", "keep-alive")
	h.Set("X-Accel-Buffering", "no")
	w.WriteHeader(http.StatusOK)
	fl.Flush()

	id, ch, recent := s.hub.SubscribeMonitor()
	defer s.hub.UnsubscribeMonitor(id)

	send := func(ev hub.MonitorEvent) bool {
		b, err := json.Marshal(ev)
		if err != nil {
			return true
		}
		if _, err := fmt.Fprintf(w, "data: %s\n\n", b); err != nil {
			return false
		}
		fl.Flush()
		return true
	}
	for _, ev := range recent {
		if !send(ev) {
			return
		}
	}
	ctx := r.Context()
	for {
		select {
		case <-ctx.Done():
			return
		case ev, ok := <-ch:
			if !ok || !send(ev) {
				return
			}
		}
	}
}
