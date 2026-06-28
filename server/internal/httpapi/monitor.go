package httpapi

import (
	"encoding/json"
	"fmt"
	"image"
	"image/color"
	_ "image/gif"
	"image/jpeg"
	_ "image/png"
	"io"
	"net/http"

	"github.com/syaro/copysync/internal/blob"
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

// handleActivity returns ~17 weeks of daily clip activity for the admin "잔디"
// (contribution-graph) heatmap: per-day count + byte totals, plus the maxima for
// color scaling.
func (s *Server) handleActivity(w http.ResponseWriter, r *http.Request) {
	days, _ := s.store.ActivitySince(s.now(), 119)
	var maxC, maxB int64
	for _, d := range days {
		if d.Count > maxC {
			maxC = d.Count
		}
		if d.Bytes > maxB {
			maxB = d.Bytes
		}
	}
	writeJSON(w, http.StatusOK, map[string]any{"days": days, "maxCount": maxC, "maxBytes": maxB})
}

// handleMonitorBlob serves a small JPEG thumbnail of a (non-E2E) image blob for
// the admin Monitoring feed. The blob endpoint proper requires a device token;
// this admin-session route lets the operator preview images the server holds.
func (s *Server) handleMonitorBlob(w http.ResponseWriter, r *http.Request) {
	if s.blobStore == nil {
		http.Error(w, "blob channel disabled", http.StatusServiceUnavailable)
		return
	}
	id := r.PathValue("id")
	if !blob.ValidID(id) {
		http.Error(w, "bad id", http.StatusBadRequest)
		return
	}
	rc, _, err := s.blobStore.Open(id)
	if err != nil {
		http.Error(w, "not found", http.StatusNotFound) // e.g. on-demand, not uploaded yet
		return
	}
	defer func() { _ = rc.Close() }()
	// Guard against a decompression bomb: the blob's bytes are uploaded by a paired
	// (low-privilege) device, and PUT /blob only checks the sha256 + encoded size,
	// NOT the decoded pixel count. A few-KB PNG/GIF can declare enormous dimensions,
	// and image.Decode would allocate width*height*4 bytes BEFORE we downscale,
	// OOM-killing the relay. Read only the header first (no pixel buffer allocated)
	// and reject oversized images, then rewind and decode for real.
	cfg, _, cerr := image.DecodeConfig(rc)
	if cerr != nil {
		http.Error(w, "not a decodable image", http.StatusUnsupportedMediaType)
		return
	}
	const maxMegapixels = 64
	if cfg.Width <= 0 || cfg.Height <= 0 || int64(cfg.Width)*int64(cfg.Height) > maxMegapixels*1_000_000 {
		http.Error(w, "image too large", http.StatusUnprocessableEntity)
		return
	}
	if _, err := rc.Seek(0, io.SeekStart); err != nil {
		http.Error(w, "io error", http.StatusInternalServerError)
		return
	}
	img, _, derr := image.Decode(rc)
	if derr != nil {
		http.Error(w, "not a decodable image", http.StatusUnsupportedMediaType)
		return
	}
	w.Header().Set("Content-Type", "image/jpeg")
	w.Header().Set("Cache-Control", "private, max-age=86400, immutable") // content-addressed
	_ = jpeg.Encode(w, downscale(img, 256), &jpeg.Options{Quality: 80})
}

// downscale box-averages src down so its longest side is <= max (stdlib only).
func downscale(src image.Image, max int) image.Image {
	b := src.Bounds()
	w, h := b.Dx(), b.Dy()
	if w <= 0 || h <= 0 {
		return src
	}
	nw, nh := w, h
	if w > max || h > max {
		if w >= h {
			nw, nh = max, h*max/w
		} else {
			nh, nw = max, w*max/h
		}
	}
	if nw < 1 {
		nw = 1
	}
	if nh < 1 {
		nh = 1
	}
	if nw == w && nh == h {
		return src
	}
	dst := image.NewRGBA(image.Rect(0, 0, nw, nh))
	for dy := 0; dy < nh; dy++ {
		sy0, sy1 := b.Min.Y+dy*h/nh, b.Min.Y+(dy+1)*h/nh
		if sy1 <= sy0 {
			sy1 = sy0 + 1
		}
		for dx := 0; dx < nw; dx++ {
			sx0, sx1 := b.Min.X+dx*w/nw, b.Min.X+(dx+1)*w/nw
			if sx1 <= sx0 {
				sx1 = sx0 + 1
			}
			var rr, gg, bb, aa, n uint64
			for sy := sy0; sy < sy1; sy++ {
				for sx := sx0; sx < sx1; sx++ {
					cr, cg, cb, ca := src.At(sx, sy).RGBA()
					rr, gg, bb, aa, n = rr+uint64(cr), gg+uint64(cg), bb+uint64(cb), aa+uint64(ca), n+1
				}
			}
			if n == 0 {
				n = 1
			}
			dst.SetRGBA(dx, dy, color.RGBA{uint8(rr / n >> 8), uint8(gg / n >> 8), uint8(bb / n >> 8), uint8(aa / n >> 8)})
		}
	}
	return dst
}
