package store

import (
	"encoding/json"
	"time"

	"github.com/syaro/copysync/internal/model"
	bolt "go.etcd.io/bbolt"
)

// PutBlobEntry stores blob metadata.
func (s *Store) PutBlobEntry(e model.BlobEntry) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		return putJSON(tx.Bucket(bucketBlobs), []byte(e.ID), e)
	})
}

// GetBlobEntry returns blob metadata by id.
func (s *Store) GetBlobEntry(id model.BlobID) (model.BlobEntry, bool, error) {
	var e model.BlobEntry
	var found bool
	err := s.db.View(func(tx *bolt.Tx) error {
		var er error
		found, er = getJSON(tx.Bucket(bucketBlobs), []byte(id), &e)
		return er
	})
	return e, found, err
}

// ListBlobEntries returns all blob metadata records.
func (s *Store) ListBlobEntries() ([]model.BlobEntry, error) {
	out := []model.BlobEntry{}
	err := s.db.View(func(tx *bolt.Tx) error {
		return tx.Bucket(bucketBlobs).ForEach(func(_, v []byte) error {
			var e model.BlobEntry
			if err := json.Unmarshal(v, &e); err != nil {
				return err
			}
			out = append(out, e)
			return nil
		})
	})
	return out, err
}

// DeleteBlobEntry removes blob metadata.
func (s *Store) DeleteBlobEntry(id model.BlobID) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		return tx.Bucket(bucketBlobs).Delete([]byte(id))
	})
}

// SetBlobACL persists the fetch ACL (origin holder + the recipient set) onto the
// blob's metadata record, creating the entry if it does not exist yet (like
// TouchBlob). This lets a GET /blob/<id> stay authorizable after the origin
// disconnects and its in-memory ACL is pruned. CreatedAt is stamped on first
// sight so a later TTL sweep can age the record out; the ACL is reclaimed with
// the whole record when DeleteBlobEntry runs during GC.
func (s *Store) SetBlobACL(id model.BlobID, origin model.DeviceID, allowed []model.DeviceID) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		b := tx.Bucket(bucketBlobs)
		var e model.BlobEntry
		found, _ := getJSON(b, []byte(id), &e)
		if !found {
			e = model.BlobEntry{ID: id}
		}
		e.Origin = origin
		e.Allowed = allowed
		return putJSON(b, []byte(id), e)
	})
}

// TouchBlob records (or refreshes) a blob's metadata: it sets CreatedAt on first
// sight, always bumps LastAccess, and fills size/mime when provided.
func (s *Store) TouchBlob(id model.BlobID, now time.Time, size int64, mime string) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		b := tx.Bucket(bucketBlobs)
		var e model.BlobEntry
		found, _ := getJSON(b, []byte(id), &e)
		if !found {
			e = model.BlobEntry{ID: id, CreatedAt: now}
		}
		if e.CreatedAt.IsZero() {
			e.CreatedAt = now
		}
		e.LastAccess = now
		if size > 0 {
			e.Size = size
		}
		if mime != "" {
			e.Mime = mime
		}
		return putJSON(b, []byte(id), e)
	})
}
