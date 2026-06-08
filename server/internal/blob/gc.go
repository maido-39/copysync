package blob

import (
	"sort"
	"time"

	"github.com/syaro/copysync/internal/model"
)

// MetaStore is the metadata access the garbage collector needs. It is satisfied
// by the bbolt store.
type MetaStore interface {
	ListBlobEntries() ([]model.BlobEntry, error)
	DeleteBlobEntry(model.BlobID) error
	// AllQueuedBlobIDs returns the set of blob ids referenced by any queued clip;
	// these are "pinned" and must never be collected.
	AllQueuedBlobIDs() (map[model.BlobID]struct{}, error)
}

// GCConfig controls a single garbage-collection pass.
type GCConfig struct {
	Now      time.Time
	BlobTTL  time.Duration
	StoreCap int64
}

// RunGC removes blobs that are not pinned by any queued clip: first those whose
// last access is older than BlobTTL, then — if total usage still exceeds
// StoreCap — the least-recently-accessed ones until under the cap. Pinning is
// derived freshly from the queues each pass, so it cannot drift.
func RunGC(fs *FsBlobStore, meta MetaStore, cfg GCConfig) (deleted int, err error) {
	pinned, err := meta.AllQueuedBlobIDs()
	if err != nil {
		return 0, err
	}
	entries, err := meta.ListBlobEntries()
	if err != nil {
		return 0, err
	}

	isPinned := func(id model.BlobID) bool { _, ok := pinned[id]; return ok }

	// Pass 1: TTL expiry + self-heal of entries whose file vanished.
	survivors := make([]model.BlobEntry, 0, len(entries))
	for _, e := range entries {
		if isPinned(e.ID) {
			survivors = append(survivors, e)
			continue
		}
		if _, ok := fs.Has(string(e.ID)); !ok {
			_ = meta.DeleteBlobEntry(e.ID) // stale metadata, no file
			continue
		}
		if cfg.BlobTTL > 0 && cfg.Now.Sub(e.LastAccess) > cfg.BlobTTL {
			_ = fs.Delete(string(e.ID))
			_ = meta.DeleteBlobEntry(e.ID)
			deleted++
			continue
		}
		survivors = append(survivors, e)
	}

	// Pass 2: enforce the total size cap via LRU eviction of unpinned survivors.
	if cfg.StoreCap > 0 {
		usage, _ := fs.DiskUsage()
		if usage > cfg.StoreCap {
			evictable := make([]model.BlobEntry, 0, len(survivors))
			for _, e := range survivors {
				if !isPinned(e.ID) {
					evictable = append(evictable, e)
				}
			}
			sort.Slice(evictable, func(i, j int) bool {
				return evictable[i].LastAccess.Before(evictable[j].LastAccess)
			})
			for _, e := range evictable {
				if usage <= cfg.StoreCap {
					break
				}
				sz := e.Size
				if sz == 0 {
					if s2, ok := fs.Has(string(e.ID)); ok {
						sz = s2
					}
				}
				_ = fs.Delete(string(e.ID))
				_ = meta.DeleteBlobEntry(e.ID)
				usage -= sz
				deleted++
			}
		}
	}
	return deleted, nil
}
