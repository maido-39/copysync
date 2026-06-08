package httpapi

import (
	"context"
	"errors"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/syaro/copysync/internal/blob"
	"github.com/syaro/copysync/internal/model"
)

const onDemandTimeout = 60 * time.Second

var (
	errNoOrigin    = errors.New("no online origin for blob")
	errPullTimeout = errors.New("on-demand pull timed out")
)

// blobWaiters lets a GET that missed block until a matching PUT arrives (the
// on-demand pull: the origin device uploads the blob after a blob_request).
type blobWaiters struct {
	mu sync.Mutex
	m  map[string][]chan struct{}
}

func newBlobWaiters() *blobWaiters { return &blobWaiters{m: make(map[string][]chan struct{})} }

func (b *blobWaiters) add(id string) chan struct{} {
	ch := make(chan struct{})
	b.mu.Lock()
	b.m[id] = append(b.m[id], ch)
	b.mu.Unlock()
	return ch
}

func (b *blobWaiters) remove(id string, ch chan struct{}) {
	b.mu.Lock()
	defer b.mu.Unlock()
	w := b.m[id]
	for i, c := range w {
		if c == ch {
			b.m[id] = append(w[:i], w[i+1:]...)
			break
		}
	}
	if len(b.m[id]) == 0 {
		delete(b.m, id)
	}
}

func (b *blobWaiters) signal(id string) {
	b.mu.Lock()
	defer b.mu.Unlock()
	for _, ch := range b.m[id] {
		close(ch)
	}
	delete(b.m, id)
}

// pullOnDemand asks the origin device to upload the blob, then waits for it.
func (s *Server) pullOnDemand(ctx context.Context, id string) (io.ReadSeekCloser, error) {
	if s.hub == nil {
		return nil, errNoOrigin
	}
	ch := s.blobWaiters.add(id)
	defer s.blobWaiters.remove(id, ch)
	if !s.hub.RequestBlob(model.BlobID(id)) {
		return nil, errNoOrigin
	}
	// It may have arrived between the failed Open and registering the waiter.
	if rc, _, err := s.blobStore.Open(id); err == nil {
		return rc, nil
	}
	select {
	case <-ch:
		rc, _, err := s.blobStore.Open(id)
		if err != nil {
			return nil, errPullTimeout
		}
		return rc, nil
	case <-time.After(onDemandTimeout):
		return nil, errPullTimeout
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// authBlob authenticates a blob-channel request by its bearer token alone
// (resolved to a device via the token-hash index).
func (s *Server) authBlob(r *http.Request) (model.Device, bool) {
	const prefix = "Bearer "
	a := r.Header.Get("Authorization")
	if !strings.HasPrefix(a, prefix) || s.validateBlobToken == nil {
		return model.Device{}, false
	}
	token := strings.TrimPrefix(a, prefix)
	if token == "" {
		return model.Device{}, false
	}
	return s.validateBlobToken(token)
}

func (s *Server) handleBlobPut(w http.ResponseWriter, r *http.Request) {
	if s.blobStore == nil {
		writeJSONError(w, http.StatusServiceUnavailable, "unavailable", "blob channel disabled")
		return
	}
	if _, ok := s.authBlob(r); !ok {
		writeJSONError(w, http.StatusUnauthorized, "unauthorized", "invalid device token")
		return
	}
	id := r.PathValue("id")
	if !blob.ValidID(id) {
		writeJSONError(w, http.StatusBadRequest, "bad_id", "id must be sha256:<hex>")
		return
	}
	settings, _ := s.store.GetSettings()
	if r.ContentLength > settings.BlobMaxBytes {
		writeJSONError(w, http.StatusRequestEntityTooLarge, "too_large", "blob exceeds cap")
		return
	}
	size, err := s.blobStore.Put(id, r.Body, settings.BlobMaxBytes)
	if err != nil {
		switch {
		case errors.Is(err, blob.ErrHashMismatch):
			writeJSONError(w, http.StatusBadRequest, "hash_mismatch", "content does not match id")
		case errors.Is(err, blob.ErrTooLarge):
			writeJSONError(w, http.StatusRequestEntityTooLarge, "too_large", "blob exceeds cap")
		default:
			writeJSONError(w, http.StatusInternalServerError, "internal", "could not store blob")
		}
		return
	}
	_ = s.store.TouchBlob(model.BlobID(id), s.now(), size, r.Header.Get("Content-Type"))
	s.blobWaiters.signal(id) // wake any on-demand GET waiting for this blob
	writeJSON(w, http.StatusCreated, map[string]any{"id": id, "size": size})
}

// handleBlobGet serves GET and HEAD for /blob/{id} (http.ServeContent handles
// both HEAD and Range requests).
func (s *Server) handleBlobGet(w http.ResponseWriter, r *http.Request) {
	if s.blobStore == nil {
		writeJSONError(w, http.StatusServiceUnavailable, "unavailable", "blob channel disabled")
		return
	}
	if _, ok := s.authBlob(r); !ok {
		writeJSONError(w, http.StatusUnauthorized, "unauthorized", "invalid device token")
		return
	}
	id := r.PathValue("id")
	if !blob.ValidID(id) {
		writeJSONError(w, http.StatusBadRequest, "bad_id", "id must be sha256:<hex>")
		return
	}
	rc, _, err := s.blobStore.Open(id)
	if err != nil {
		// Not stored yet — try to pull it on demand from the origin device.
		rc, err = s.pullOnDemand(r.Context(), id)
		if err != nil {
			if errors.Is(err, errPullTimeout) {
				writeJSONError(w, http.StatusGatewayTimeout, "timeout", "source did not provide the file in time")
			} else if errors.Is(err, context.Canceled) {
				return
			} else {
				writeJSONError(w, http.StatusNotFound, "not_found", "blob not found")
			}
			return
		}
	}
	defer func() { _ = rc.Close() }()
	_ = s.store.TouchBlob(model.BlobID(id), s.now(), 0, "")
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Header().Set("ETag", `"`+id+`"`)
	http.ServeContent(w, r, "", time.Time{}, rc)
}
