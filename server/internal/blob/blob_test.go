package blob_test

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"strings"
	"testing"
	"time"

	"github.com/syaro/copysync/internal/blob"
	"github.com/syaro/copysync/internal/model"
)

func idFor(content string) string {
	sum := sha256.Sum256([]byte(content))
	return "sha256:" + hex.EncodeToString(sum[:])
}

func TestPutGetRoundTrip(t *testing.T) {
	fs, err := blob.NewFsBlobStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	content := "hello blob world"
	id := idFor(content)
	n, err := fs.Put(id, strings.NewReader(content), 1<<20)
	if err != nil || n != int64(len(content)) {
		t.Fatalf("put: n=%d err=%v", n, err)
	}
	// Put is idempotent.
	if _, err := fs.Put(id, strings.NewReader(content), 1<<20); err != nil {
		t.Fatalf("idempotent put: %v", err)
	}
	rc, size, err := fs.Open(id)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer rc.Close()
	got, _ := io.ReadAll(rc)
	if size != int64(len(content)) || string(got) != content {
		t.Fatalf("read back %q (size %d)", got, size)
	}
}

func TestPutHashMismatch(t *testing.T) {
	fs, _ := blob.NewFsBlobStore(t.TempDir())
	id := idFor("expected")
	_, err := fs.Put(id, strings.NewReader("different"), 1<<20)
	if !errors.Is(err, blob.ErrHashMismatch) {
		t.Fatalf("err = %v, want ErrHashMismatch", err)
	}
	if _, ok := fs.Has(id); ok {
		t.Fatal("blob must not exist after a hash mismatch")
	}
}

func TestPutTooLarge(t *testing.T) {
	fs, _ := blob.NewFsBlobStore(t.TempDir())
	content := strings.Repeat("x", 100)
	if _, err := fs.Put(idFor(content), strings.NewReader(content), 50); !errors.Is(err, blob.ErrTooLarge) {
		t.Fatalf("err = %v, want ErrTooLarge", err)
	}
}

func TestValidID(t *testing.T) {
	if !blob.ValidID(idFor("x")) {
		t.Fatal("valid id rejected")
	}
	if blob.ValidID("sha256:xyz") || blob.ValidID("md5:"+strings.Repeat("a", 32)) {
		t.Fatal("malformed id accepted")
	}
}

// fakeMeta implements blob.MetaStore for GC tests.
type fakeMeta struct {
	entries map[model.BlobID]model.BlobEntry
	pinned  map[model.BlobID]struct{}
}

func (f *fakeMeta) ListBlobEntries() ([]model.BlobEntry, error) {
	out := make([]model.BlobEntry, 0, len(f.entries))
	for _, e := range f.entries {
		out = append(out, e)
	}
	return out, nil
}
func (f *fakeMeta) DeleteBlobEntry(id model.BlobID) error { delete(f.entries, id); return nil }
func (f *fakeMeta) AllQueuedBlobIDs() (map[model.BlobID]struct{}, error) {
	return f.pinned, nil
}

func TestRunGCTTLAndPin(t *testing.T) {
	fs, _ := blob.NewFsBlobStore(t.TempDir())
	put := func(content string) model.BlobID {
		id := idFor(content)
		if _, err := fs.Put(id, strings.NewReader(content), 1<<20); err != nil {
			t.Fatal(err)
		}
		return model.BlobID(id)
	}
	now := time.Now()
	pinned, fresh, stale := put("pinned-blob"), put("fresh-blob-1"), put("stale-blob-1")
	meta := &fakeMeta{
		entries: map[model.BlobID]model.BlobEntry{
			pinned: {ID: pinned, LastAccess: now.Add(-48 * time.Hour)}, // old but pinned
			fresh:  {ID: fresh, LastAccess: now.Add(-1 * time.Hour)},
			stale:  {ID: stale, LastAccess: now.Add(-48 * time.Hour)},
		},
		pinned: map[model.BlobID]struct{}{pinned: {}},
	}
	deleted, err := blob.RunGC(fs, meta, blob.GCConfig{Now: now, BlobTTL: 24 * time.Hour})
	if err != nil || deleted != 1 {
		t.Fatalf("deleted=%d err=%v want 1/nil", deleted, err)
	}
	if _, ok := fs.Has(string(stale)); ok {
		t.Fatal("stale blob should be deleted")
	}
	if _, ok := fs.Has(string(pinned)); !ok {
		t.Fatal("pinned blob must survive past TTL")
	}
	if _, ok := fs.Has(string(fresh)); !ok {
		t.Fatal("fresh blob must survive")
	}
	if _, ok := meta.entries[stale]; ok {
		t.Fatal("stale metadata entry should be removed")
	}
}

func TestRunGCSizeCapLRU(t *testing.T) {
	fs, _ := blob.NewFsBlobStore(t.TempDir())
	put := func(content string) model.BlobID {
		id := idFor(content)
		if _, err := fs.Put(id, strings.NewReader(content), 1<<20); err != nil {
			t.Fatal(err)
		}
		return model.BlobID(id)
	}
	now := time.Now()
	a := put(strings.Repeat("a", 100))
	b := put(strings.Repeat("b", 100))
	c := put(strings.Repeat("c", 100))
	meta := &fakeMeta{
		entries: map[model.BlobID]model.BlobEntry{
			a: {ID: a, Size: 100, LastAccess: now.Add(-3 * time.Hour)}, // LRU → evicted
			b: {ID: b, Size: 100, LastAccess: now.Add(-2 * time.Hour)},
			c: {ID: c, Size: 100, LastAccess: now.Add(-1 * time.Hour)},
		},
		pinned: map[model.BlobID]struct{}{},
	}
	deleted, err := blob.RunGC(fs, meta, blob.GCConfig{Now: now, BlobTTL: 24 * time.Hour, StoreCap: 250})
	if err != nil || deleted != 1 {
		t.Fatalf("deleted=%d err=%v want 1/nil", deleted, err)
	}
	if _, ok := fs.Has(string(a)); ok {
		t.Fatal("least-recently-accessed blob should be evicted under cap")
	}
	if _, ok := fs.Has(string(b)); !ok {
		t.Fatal("b should remain")
	}
	if _, ok := fs.Has(string(c)); !ok {
		t.Fatal("c should remain")
	}
}
