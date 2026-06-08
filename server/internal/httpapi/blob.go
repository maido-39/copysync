package httpapi

import (
	"errors"
	"net/http"
	"strings"
	"time"

	"github.com/syaro/copysync/internal/blob"
	"github.com/syaro/copysync/internal/model"
)

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
		writeJSONError(w, http.StatusNotFound, "not_found", "blob not found")
		return
	}
	defer func() { _ = rc.Close() }()
	_ = s.store.TouchBlob(model.BlobID(id), s.now(), 0, "")
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Header().Set("ETag", `"`+id+`"`)
	http.ServeContent(w, r, "", time.Time{}, rc)
}
