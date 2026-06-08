// Package store is the only package that touches bbolt. It persists the device
// registry, tokens, pairing codes, the admin account, sessions, runtime
// settings, blob metadata, and the bounded per-device offline queues.
package store

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"time"

	bolt "go.etcd.io/bbolt"
)

// Store wraps a bbolt database.
type Store struct {
	db *bolt.DB
}

// Open opens (or creates) <dataDir>/copysync.db and ensures all buckets exist.
func Open(dataDir string) (*Store, error) {
	path := filepath.Join(dataDir, "copysync.db")
	db, err := bolt.Open(path, 0o600, &bolt.Options{Timeout: 3 * time.Second})
	if err != nil {
		return nil, fmt.Errorf("open bbolt at %s: %w", path, err)
	}
	s := &Store{db: db}
	if err := s.init(); err != nil {
		_ = db.Close()
		return nil, err
	}
	return s, nil
}

func (s *Store) init() error {
	return s.db.Update(func(tx *bolt.Tx) error {
		for _, b := range allBuckets {
			if _, err := tx.CreateBucketIfNotExists(b); err != nil {
				return err
			}
		}
		return nil
	})
}

// Close closes the database.
func (s *Store) Close() error { return s.db.Close() }

func putJSON(b *bolt.Bucket, key []byte, v any) error {
	data, err := json.Marshal(v)
	if err != nil {
		return err
	}
	return b.Put(key, data)
}

func getJSON(b *bolt.Bucket, key []byte, v any) (bool, error) {
	data := b.Get(key)
	if data == nil {
		return false, nil
	}
	return true, json.Unmarshal(data, v)
}
