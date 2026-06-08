package store

import (
	"encoding/json"
	"time"

	"github.com/syaro/copysync/internal/model"
	bolt "go.etcd.io/bbolt"
)

var keyAdminUser = []byte("user")

// GetAdmin returns the admin user, if one has been seeded.
func (s *Store) GetAdmin() (model.AdminUser, bool, error) {
	var a model.AdminUser
	var found bool
	err := s.db.View(func(tx *bolt.Tx) error {
		var e error
		found, e = getJSON(tx.Bucket(bucketAdmin), keyAdminUser, &a)
		return e
	})
	return a, found, err
}

// PutAdmin stores the admin user.
func (s *Store) PutAdmin(a model.AdminUser) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		return putJSON(tx.Bucket(bucketAdmin), keyAdminUser, a)
	})
}

// PutSession stores a session keyed by its id hash.
func (s *Store) PutSession(sess model.Session) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		return putJSON(tx.Bucket(bucketSessions), []byte(sess.IDHash), sess)
	})
}

// GetSession returns a session by its id hash.
func (s *Store) GetSession(idHash string) (model.Session, bool, error) {
	var sess model.Session
	var found bool
	err := s.db.View(func(tx *bolt.Tx) error {
		var e error
		found, e = getJSON(tx.Bucket(bucketSessions), []byte(idHash), &sess)
		return e
	})
	return sess, found, err
}

// DeleteSession removes a session.
func (s *Store) DeleteSession(idHash string) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		return tx.Bucket(bucketSessions).Delete([]byte(idHash))
	})
}

// PurgeExpiredSessions deletes sessions past their expiry.
func (s *Store) PurgeExpiredSessions(now time.Time) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		b := tx.Bucket(bucketSessions)
		var stale [][]byte
		err := b.ForEach(func(k, v []byte) error {
			var sess model.Session
			if json.Unmarshal(v, &sess) == nil && now.After(sess.ExpiresAt) {
				stale = append(stale, append([]byte(nil), k...))
			}
			return nil
		})
		if err != nil {
			return err
		}
		for _, k := range stale {
			_ = b.Delete(k)
		}
		return nil
	})
}
