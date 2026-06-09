package store

import (
	"encoding/json"
	"time"

	bolt "go.etcd.io/bbolt"
)

// DayStat is one day's clipboard activity (for the admin "잔디" heatmap).
type DayStat struct {
	Date  string `json:"date"` // YYYY-MM-DD
	Count int64  `json:"count"`
	Bytes int64  `json:"bytes"`
}

func dayKey(t time.Time) []byte { return []byte(t.Format("2006-01-02")) }

// RecordActivity bumps the given day's clip count and byte total.
func (s *Store) RecordActivity(t time.Time, bytes int64) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		b := tx.Bucket(bucketActivity)
		key := dayKey(t)
		var d DayStat
		_, _ = getJSON(b, key, &d)
		d.Date = string(key)
		d.Count++
		if bytes > 0 {
			d.Bytes += bytes
		}
		return putJSON(b, key, d)
	})
}

// ActivitySince returns one DayStat per day for the last `days` days (oldest
// first), zero-filling days with no activity.
func (s *Store) ActivitySince(now time.Time, days int) ([]DayStat, error) {
	if days < 1 {
		days = 1
	}
	seen := make(map[string]DayStat, days)
	err := s.db.View(func(tx *bolt.Tx) error {
		return tx.Bucket(bucketActivity).ForEach(func(k, v []byte) error {
			var d DayStat
			if json.Unmarshal(v, &d) == nil {
				seen[string(k)] = d
			}
			return nil
		})
	})
	out := make([]DayStat, 0, days)
	start := now.AddDate(0, 0, -(days - 1))
	for i := 0; i < days; i++ {
		key := start.AddDate(0, 0, i).Format("2006-01-02")
		if d, ok := seen[key]; ok {
			out = append(out, d)
		} else {
			out = append(out, DayStat{Date: key})
		}
	}
	return out, err
}
