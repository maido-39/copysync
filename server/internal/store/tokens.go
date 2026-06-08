package store

import (
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
