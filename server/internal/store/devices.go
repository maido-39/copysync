package store

import (
	"encoding/json"
	"strings"
	"time"

	"github.com/syaro/copysync/internal/model"
	bolt "go.etcd.io/bbolt"
)

// PutDevice stores a device and indexes its lowercased name for uniqueness.
func (s *Store) PutDevice(d model.Device) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		if err := putJSON(tx.Bucket(bucketDevices), []byte(d.ID), d); err != nil {
			return err
		}
		return tx.Bucket(bucketDeviceNames).Put([]byte(strings.ToLower(d.Name)), []byte(d.ID))
	})
}

// GetDevice returns a device by id.
func (s *Store) GetDevice(id model.DeviceID) (model.Device, bool, error) {
	var d model.Device
	var found bool
	err := s.db.View(func(tx *bolt.Tx) error {
		var e error
		found, e = getJSON(tx.Bucket(bucketDevices), []byte(id), &d)
		return e
	})
	return d, found, err
}

// ListDevices returns all devices.
func (s *Store) ListDevices() ([]model.Device, error) {
	out := []model.Device{}
	err := s.db.View(func(tx *bolt.Tx) error {
		return tx.Bucket(bucketDevices).ForEach(func(_, v []byte) error {
			var d model.Device
			if err := json.Unmarshal(v, &d); err != nil {
				return err
			}
			out = append(out, d)
			return nil
		})
	})
	return out, err
}

// DeviceNameTaken reports whether a device name is in use by a device other than
// exclude.
func (s *Store) DeviceNameTaken(name string, exclude model.DeviceID) (bool, error) {
	var taken bool
	err := s.db.View(func(tx *bolt.Tx) error {
		if v := tx.Bucket(bucketDeviceNames).Get([]byte(strings.ToLower(name))); v != nil {
			taken = model.DeviceID(v) != exclude
		}
		return nil
	})
	return taken, err
}

// UpdateLastSeen sets a device's last-seen timestamp (no-op if device is gone).
func (s *Store) UpdateLastSeen(id model.DeviceID, t time.Time) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		b := tx.Bucket(bucketDevices)
		var d model.Device
		found, err := getJSON(b, []byte(id), &d)
		if err != nil || !found {
			return err
		}
		d.LastSeenAt = t
		return putJSON(b, []byte(id), d)
	})
}

// DeleteDevice removes a device, its name index, its token, and its offline queue.
func (s *Store) DeleteDevice(id model.DeviceID) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		b := tx.Bucket(bucketDevices)
		var d model.Device
		found, err := getJSON(b, []byte(id), &d)
		if err != nil {
			return err
		}
		if found {
			_ = tx.Bucket(bucketDeviceNames).Delete([]byte(strings.ToLower(d.Name)))
		}
		_ = b.Delete([]byte(id))
		var tr model.TokenRecord
		if ok, _ := getJSON(tx.Bucket(bucketTokens), []byte(id), &tr); ok {
			_ = tx.Bucket(bucketTokenIndex).Delete([]byte(tr.TokenHash))
			// During a pending rotation the index also holds a PrevHash->id entry
			// (added by RotateToken, normally removed by RetireOldToken). Delete it
			// too, otherwise it dangles forever once the token record is gone.
			if tr.PrevHash != "" {
				_ = tx.Bucket(bucketTokenIndex).Delete([]byte(tr.PrevHash))
			}
		}
		_ = tx.Bucket(bucketTokens).Delete([]byte(id))
		if qb := tx.Bucket(bucketQueues).Bucket([]byte(id)); qb != nil {
			_ = tx.Bucket(bucketQueues).DeleteBucket([]byte(id))
		}
		return nil
	})
}
