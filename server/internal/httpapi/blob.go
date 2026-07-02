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
	"github.com/syaro/copysync/internal/hub"
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
	dev, ok := s.authBlob(r)
	if !ok {
		writeJSONError(w, http.StatusUnauthorized, "unauthorized", "invalid device token")
		return
	}
	id := r.PathValue("id")
	if !blob.ValidID(id) {
		writeJSONError(w, http.StatusBadRequest, "bad_id", "id must be sha256:<hex>")
		return
	}
	// Only the recorded on-demand origin holder may supply a blob's bytes, so a
	// paired device cannot inject content for a blob id it never advertised.
	if s.hub != nil && !s.hub.AuthorizedForBlob(model.BlobID(id), dev.ID, hub.BlobSupply) {
		// 404 rather than 403 so the response does not confirm the id exists.
		s.dbgBlob("blob_put-denied", "device", dev.ID, "id", id, "reason", "not the on-demand origin holder")
		writeJSONError(w, http.StatusNotFound, "not_found", "blob not found")
		return
	}
	settings, _ := s.store.GetSettings()
	if r.ContentLength > settings.BlobMaxBytes {
		s.dbgBlob("blob_put-denied", "device", dev.ID, "id", id, "reason", "content-length exceeds cap", "size", r.ContentLength, "cap", settings.BlobMaxBytes)
		writeJSONError(w, http.StatusRequestEntityTooLarge, "too_large", "blob exceeds cap")
		return
	}
	size, err := s.blobStore.Put(id, r.Body, settings.BlobMaxBytes)
	if err != nil {
		switch {
		case errors.Is(err, blob.ErrHashMismatch):
			s.dbgBlob("blob_put-denied", "device", dev.ID, "id", id, "reason", "hash mismatch")
			writeJSONError(w, http.StatusBadRequest, "hash_mismatch", "content does not match id")
		case errors.Is(err, blob.ErrTooLarge):
			s.dbgBlob("blob_put-denied", "device", dev.ID, "id", id, "reason", "blob exceeds cap (during copy)")
			writeJSONError(w, http.StatusRequestEntityTooLarge, "too_large", "blob exceeds cap")
		default:
			s.dbgBlob("blob_put-error", "device", dev.ID, "id", id, "err", err)
			writeJSONError(w, http.StatusInternalServerError, "internal", "could not store blob")
		}
		return
	}
	_ = s.store.TouchBlob(model.BlobID(id), s.now(), size, r.Header.Get("Content-Type"))
	s.blobWaiters.signal(id) // wake any on-demand GET waiting for this blob
	s.dbgBlob("blob_put-accepted", "device", dev.ID, "id", id, "size", size)
	writeJSON(w, http.StatusCreated, map[string]any{"id": id, "size": size})
}

// handleBlobGet serves GET and HEAD for /blob/{id} (http.ServeContent handles
// both HEAD and Range requests).
func (s *Server) handleBlobGet(w http.ResponseWriter, r *http.Request) {
	if s.blobStore == nil {
		writeJSONError(w, http.StatusServiceUnavailable, "unavailable", "blob channel disabled")
		return
	}
	dev, ok := s.authBlob(r)
	if !ok {
		writeJSONError(w, http.StatusUnauthorized, "unauthorized", "invalid device token")
		return
	}
	id := r.PathValue("id")
	if !blob.ValidID(id) {
		writeJSONError(w, http.StatusBadRequest, "bad_id", "id must be sha256:<hex>")
		return
	}
	// Authorize against the clip that referenced this blob: a device may only
	// fetch a blob it was a recipient of (origin ∪ targets / origin's pool). This
	// also prevents an unauthorized requester from coercing pullOnDemand into
	// waking the origin to upload bytes it only shared within another pool.
	authMode := "hub-acl"
	authorized := s.hub != nil && s.hub.AuthorizedForBlob(model.BlobID(id), dev.ID, hub.BlobFetch)
	if !authorized {
		// The hub's in-memory ACL is pruned when the origin disconnects, so a fetch
		// for an offline-queued or send-and-exit blob would 404 while the bytes sit
		// on disk. Fall back to the ACL persisted on the blob record (origin ∪
		// targets), which survives the origin going offline. Runs in the HTTP
		// goroutine (not the hub Run goroutine), so it reads the store directly.
		if entry, found, err := s.store.GetBlobEntry(model.BlobID(id)); err == nil && found {
			if dev.ID == entry.Origin {
				authorized = true
				authMode = "persisted-acl-origin"
			} else {
				for _, a := range entry.Allowed {
					if a == dev.ID {
						authorized = true
						authMode = "persisted-acl-allowed"
						break
					}
				}
			}
		}
	}
	if !authorized {
		// 404 rather than 403 so the response does not confirm the id exists.
		s.dbgBlob("blob_get-denied", "device", dev.ID, "id", id, "reason", "not in ACL (hub or persisted)")
		writeJSONError(w, http.StatusNotFound, "not_found", "blob not found")
		return
	}
	rc, _, err := s.blobStore.Open(id)
	pulled := false
	if err != nil {
		// Not stored yet — try to pull it on demand from the origin device.
		rc, err = s.pullOnDemand(r.Context(), id)
		if err != nil {
			switch {
			case errors.Is(err, errPullTimeout):
				s.dbgBlob("blob_get-404", "device", dev.ID, "id", id, "auth_mode", authMode, "reason", "on-demand pull timed out")
				writeJSONError(w, http.StatusGatewayTimeout, "timeout", "source did not provide the file in time")
			case errors.Is(err, context.Canceled):
				return
			default:
				s.dbgBlob("blob_get-404", "device", dev.ID, "id", id, "auth_mode", authMode, "reason", "not stored and no online origin")
				writeJSONError(w, http.StatusNotFound, "not_found", "blob not found")
			}
			return
		}
		pulled = true
	}
	defer func() { _ = rc.Close() }()
	_ = s.store.TouchBlob(model.BlobID(id), s.now(), 0, "")
	s.dbgBlob("blob_get-served", "device", dev.ID, "id", id, "auth_mode", authMode, "on_demand_pull", pulled)
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Header().Set("ETag", `"`+id+`"`)
	http.ServeContent(w, r, "", time.Time{}, rc)
}
