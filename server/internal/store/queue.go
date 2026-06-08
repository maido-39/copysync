package store

import (
	"encoding/binary"
	"encoding/json"
	"time"

	"github.com/syaro/copysync/internal/model"
	bolt "go.etcd.io/bbolt"
)

func itob(v uint64) []byte {
	b := make([]byte, 8)
	binary.BigEndian.PutUint64(b, v)
	return b
}

// Enqueue appends an item to a device's offline queue, trimming the oldest
// entries beyond maxDepth. It returns the number of items trimmed.
func (s *Store) Enqueue(id model.DeviceID, item model.QueueItem, maxDepth int) (trimmed int, err error) {
	err = s.db.Update(func(tx *bolt.Tx) error {
		qb, err := tx.Bucket(bucketQueues).CreateBucketIfNotExists([]byte(id))
		if err != nil {
			return err
		}
		seq, _ := qb.NextSequence()
		if err := putJSON(qb, itob(seq), item); err != nil {
			return err
		}
		count := 0
		_ = qb.ForEach(func(_, _ []byte) error { count++; return nil })
		for count > maxDepth {
			c := qb.Cursor()
			k, _ := c.First()
			if k == nil {
				break
			}
			if err := qb.Delete(k); err != nil {
				return err
			}
			count--
			trimmed++
		}
		return nil
	})
	return trimmed, err
}

// DrainQueue returns all queued items for a device in FIFO order and removes the
// device's queue. Returns nil if the device had no queue.
func (s *Store) DrainQueue(id model.DeviceID) ([]model.QueueItem, error) {
	var items []model.QueueItem
	err := s.db.Update(func(tx *bolt.Tx) error {
		root := tx.Bucket(bucketQueues)
		qb := root.Bucket([]byte(id))
		if qb == nil {
			return nil
		}
		err := qb.ForEach(func(_, v []byte) error {
			var it model.QueueItem
			if err := json.Unmarshal(v, &it); err != nil {
				return err
			}
			items = append(items, it)
			return nil
		})
		if err != nil {
			return err
		}
		return root.DeleteBucket([]byte(id))
	})
	return items, err
}

// AllQueuedBlobIDs returns the set of blob ids referenced by any queued item,
// across all devices. The blob GC uses this as its pin set.
func (s *Store) AllQueuedBlobIDs() (map[model.BlobID]struct{}, error) {
	set := make(map[model.BlobID]struct{})
	err := s.db.View(func(tx *bolt.Tx) error {
		root := tx.Bucket(bucketQueues)
		return root.ForEachBucket(func(k []byte) error {
			return root.Bucket(k).ForEach(func(_, v []byte) error {
				var it model.QueueItem
				if err := json.Unmarshal(v, &it); err != nil {
					return nil // skip unparseable entries
				}
				if it.Event.BlobID != "" {
					set[it.Event.BlobID] = struct{}{}
				}
				return nil
			})
		})
	})
	return set, err
}

// QueueLen returns the number of queued items for a device.
func (s *Store) QueueLen(id model.DeviceID) (int, error) {
	n := 0
	err := s.db.View(func(tx *bolt.Tx) error {
		if qb := tx.Bucket(bucketQueues).Bucket([]byte(id)); qb != nil {
			_ = qb.ForEach(func(_, _ []byte) error { n++; return nil })
		}
		return nil
	})
	return n, err
}

// PurgeExpiredQueueItems removes queued items older than ttl across all devices
// and deletes any device queue left empty. It returns the number removed. Once
// a stale item is dropped, its blob is no longer pinned and becomes eligible for
// garbage collection.
func (s *Store) PurgeExpiredQueueItems(now time.Time, ttl time.Duration) (int, error) {
	if ttl <= 0 {
		return 0, nil
	}
	removed := 0
	err := s.db.Update(func(tx *bolt.Tx) error {
		root := tx.Bucket(bucketQueues)
		type target struct{ dev, key []byte }
		var toDelete []target
		// Read phase: collect expired (device, key) pairs.
		if err := root.ForEachBucket(func(k []byte) error {
			return root.Bucket(k).ForEach(func(key, v []byte) error {
				var it model.QueueItem
				if json.Unmarshal(v, &it) == nil && now.Sub(it.EnqueuedAt) > ttl {
					toDelete = append(toDelete, target{append([]byte(nil), k...), append([]byte(nil), key...)})
				}
				return nil
			})
		}); err != nil {
			return err
		}
		// Write phase: delete collected keys.
		for _, t := range toDelete {
			if qb := root.Bucket(t.dev); qb != nil {
				if err := qb.Delete(t.key); err == nil {
					removed++
				}
			}
		}
		// Drop now-empty device queues.
		var empties [][]byte
		_ = root.ForEachBucket(func(k []byte) error {
			empty := true
			_ = root.Bucket(k).ForEach(func(_, _ []byte) error { empty = false; return nil })
			if empty {
				empties = append(empties, append([]byte(nil), k...))
			}
			return nil
		})
		for _, k := range empties {
			_ = root.DeleteBucket(k)
		}
		return nil
	})
	return removed, err
}
