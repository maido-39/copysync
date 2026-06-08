package store

import (
	"github.com/syaro/copysync/internal/config"
	bolt "go.etcd.io/bbolt"
)

var keyRuntimeSettings = []byte("runtime")

// GetSettings returns the persisted runtime settings, falling back to defaults
// for any unset value.
func (s *Store) GetSettings() (config.RuntimeSettings, error) {
	settings := config.DefaultRuntimeSettings()
	err := s.db.View(func(tx *bolt.Tx) error {
		_, e := getJSON(tx.Bucket(bucketSettings), keyRuntimeSettings, &settings)
		return e
	})
	settings.Normalize()
	return settings, err
}

// PutSettings persists runtime settings after normalizing them.
func (s *Store) PutSettings(rs config.RuntimeSettings) error {
	rs.Normalize()
	return s.db.Update(func(tx *bolt.Tx) error {
		return putJSON(tx.Bucket(bucketSettings), keyRuntimeSettings, rs)
	})
}
