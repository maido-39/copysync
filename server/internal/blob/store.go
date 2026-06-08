// Package blob is a content-addressed, filesystem-backed store for large
// clipboard payloads (images, files, rich text). Blobs are addressed by
// "sha256:<hex>" and verified on write. Metadata (size, access time) lives in
// the bbolt store; the bytes live here on disk.
package blob

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

var (
	// ErrHashMismatch means the uploaded bytes did not hash to the declared id.
	ErrHashMismatch = errors.New("blob content does not match its id")
	// ErrTooLarge means the upload exceeded the configured cap.
	ErrTooLarge = errors.New("blob exceeds size cap")
	// ErrNotFound means the blob is absent.
	ErrNotFound = errors.New("blob not found")
)

var idRe = regexp.MustCompile(`^sha256:[0-9a-f]{64}$`)

// ValidID reports whether id is a well-formed content address.
func ValidID(id string) bool { return idRe.MatchString(id) }

func hexOf(id string) string { return strings.TrimPrefix(id, "sha256:") }

// FsBlobStore stores blob bytes on the filesystem under <dataDir>/blobs, sharded
// by the first two hex characters of the digest.
type FsBlobStore struct {
	root string
}

// NewFsBlobStore prepares the blob directory (and its temp area).
func NewFsBlobStore(dataDir string) (*FsBlobStore, error) {
	root := filepath.Join(dataDir, "blobs")
	if err := os.MkdirAll(filepath.Join(root, "tmp"), 0o700); err != nil {
		return nil, err
	}
	return &FsBlobStore{root: root}, nil
}

func (s *FsBlobStore) pathFor(id string) string {
	h := hexOf(id)
	return filepath.Join(s.root, h[:2], h)
}

// Has reports whether a blob exists, returning its size.
func (s *FsBlobStore) Has(id string) (int64, bool) {
	fi, err := os.Stat(s.pathFor(id))
	if err != nil {
		return 0, false
	}
	return fi.Size(), true
}

// Put streams r into the store, verifying the content hashes to id and enforcing
// maxBytes. It is idempotent: an already-present blob returns its size without
// rewriting.
func (s *FsBlobStore) Put(id string, r io.Reader, maxBytes int64) (int64, error) {
	if !ValidID(id) {
		return 0, ErrHashMismatch
	}
	if sz, ok := s.Has(id); ok {
		_, _ = io.Copy(io.Discard, io.LimitReader(r, maxBytes+1))
		return sz, nil
	}
	tmp, err := os.CreateTemp(filepath.Join(s.root, "tmp"), "blob-*")
	if err != nil {
		return 0, err
	}
	tmpName := tmp.Name()
	defer func() { _ = os.Remove(tmpName) }() // harmless if already renamed away

	h := sha256.New()
	n, err := io.Copy(io.MultiWriter(tmp, h), io.LimitReader(r, maxBytes+1))
	if err != nil {
		_ = tmp.Close()
		return 0, err
	}
	if n > maxBytes {
		_ = tmp.Close()
		return 0, ErrTooLarge
	}
	if err := tmp.Sync(); err != nil {
		_ = tmp.Close()
		return 0, err
	}
	if err := tmp.Close(); err != nil {
		return 0, err
	}
	if hex.EncodeToString(h.Sum(nil)) != hexOf(id) {
		return 0, ErrHashMismatch
	}
	dst := s.pathFor(id)
	if err := os.MkdirAll(filepath.Dir(dst), 0o700); err != nil {
		return 0, err
	}
	if err := os.Rename(tmpName, dst); err != nil {
		return 0, err
	}
	return n, nil
}

// Open returns a seekable reader for a blob's bytes (caller must Close).
func (s *FsBlobStore) Open(id string) (io.ReadSeekCloser, int64, error) {
	f, err := os.Open(s.pathFor(id))
	if err != nil {
		return nil, 0, ErrNotFound
	}
	fi, err := f.Stat()
	if err != nil {
		_ = f.Close()
		return nil, 0, err
	}
	return f, fi.Size(), nil
}

// Delete removes a blob's bytes (best effort; missing is not an error).
func (s *FsBlobStore) Delete(id string) error {
	if err := os.Remove(s.pathFor(id)); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return nil
}

// DiskUsage returns the total bytes used by stored blobs, excluding the store's
// own temp area (only that specific directory, not any path that mentions "tmp").
func (s *FsBlobStore) DiskUsage() (int64, error) {
	var total int64
	tmpDir := filepath.Join(s.root, "tmp")
	err := filepath.WalkDir(s.root, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if d.IsDir() {
			if path == tmpDir {
				return filepath.SkipDir
			}
			return nil
		}
		if info, e := d.Info(); e == nil {
			total += info.Size()
		}
		return nil
	})
	return total, err
}
