package store

import bolt "go.etcd.io/bbolt"

// GetMeta returns a string meta value.
func (s *Store) GetMeta(key string) (string, bool, error) {
	var val string
	var found bool
	err := s.db.View(func(tx *bolt.Tx) error {
		if v := tx.Bucket(bucketMeta).Get([]byte(key)); v != nil {
			val, found = string(v), true
		}
		return nil
	})
	return val, found, err
}

// PutMeta stores a string meta value.
func (s *Store) PutMeta(key, val string) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		return tx.Bucket(bucketMeta).Put([]byte(key), []byte(val))
	})
}

// GetOrCreateMeta returns the value for key, creating it via gen() if absent.
// The read-and-create is atomic within a single transaction.
func (s *Store) GetOrCreateMeta(key string, gen func() string) (string, error) {
	var result string
	err := s.db.Update(func(tx *bolt.Tx) error {
		b := tx.Bucket(bucketMeta)
		if v := b.Get([]byte(key)); v != nil {
			result = string(v)
			return nil
		}
		result = gen()
		return b.Put([]byte(key), []byte(result))
	})
	return result, err
}
