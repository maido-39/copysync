package store

import (
	"time"

	"github.com/syaro/copysync/internal/model"
	bolt "go.etcd.io/bbolt"
)

// PutToken stores (or replaces) a device's token record.
func (s *Store) PutToken(t model.TokenRecord) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		return putJSON(tx.Bucket(bucketTokens), []byte(t.DeviceID), t)
	})
}

// GetToken returns the token record for a device.
func (s *Store) GetToken(id model.DeviceID) (model.TokenRecord, bool, error) {
	var t model.TokenRecord
	var found bool
	err := s.db.View(func(tx *bolt.Tx) error {
		var e error
		found, e = getJSON(tx.Bucket(bucketTokens), []byte(id), &t)
		return e
	})
	return t, found, err
}

// DeviceIDByTokenHash resolves a device id from a token hash, used by the blob
// channel which authenticates with the bearer token alone.
func (s *Store) DeviceIDByTokenHash(hash string) (model.DeviceID, bool, error) {
	var id model.DeviceID
	var found bool
	err := s.db.View(func(tx *bolt.Tx) error {
		if v := tx.Bucket(bucketTokenIndex).Get([]byte(hash)); v != nil {
			id, found = model.DeviceID(v), true
		}
		return nil
	})
	return id, found, err
}

// RotateToken issues a new token hash for a device while keeping the previous one
// valid (still in the index) until the device authenticates with the new one.
// No-op if a rotation is already pending (PrevHash set) or the hash is unchanged.
func (s *Store) RotateToken(id model.DeviceID, newHash string, now time.Time) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		var rec model.TokenRecord
		found, err := getJSON(tx.Bucket(bucketTokens), []byte(id), &rec)
		if err != nil || !found {
			return err
		}
		if rec.PrevHash != "" || rec.TokenHash == newHash {
			return nil
		}
		rec.PrevHash = rec.TokenHash
		rec.TokenHash = newHash
		rec.IssuedAt = now
		if err := putJSON(tx.Bucket(bucketTokens), []byte(id), rec); err != nil {
			return err
		}
		return tx.Bucket(bucketTokenIndex).Put([]byte(newHash), []byte(id))
	})
}

// RetireOldToken invalidates the previous token once the device has proven it
// holds the new one. No-op when no rotation is pending.
func (s *Store) RetireOldToken(id model.DeviceID) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		var rec model.TokenRecord
		found, err := getJSON(tx.Bucket(bucketTokens), []byte(id), &rec)
		if err != nil || !found || rec.PrevHash == "" {
			return err
		}
		if err := tx.Bucket(bucketTokenIndex).Delete([]byte(rec.PrevHash)); err != nil {
			return err
		}
		rec.PrevHash = ""
		return putJSON(tx.Bucket(bucketTokens), []byte(id), rec)
	})
}
