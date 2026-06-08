package main

import (
	"bufio"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// History is a minimal append-only local clipboard log (offline preservation +
// search), stored as JSON Lines. A full client would use SQLite FTS5; this keeps
// the reference client dependency-free while demonstrating the feature.
type History struct {
	path string
	mu   sync.Mutex
}

type histEntry struct {
	TS     time.Time `json:"ts"`
	Dir    string    `json:"dir"`    // "in" | "out"
	Origin string    `json:"origin"` // device id
	Text   string    `json:"text,omitempty"`
	Blob   string    `json:"blob,omitempty"`
}

func openHistory(path string) *History {
	if path == "" {
		path = defaultHistoryPath()
	}
	return &History{path: path}
}

func (h *History) append(dir, origin, text, blob string) {
	if h == nil {
		return
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if err := os.MkdirAll(filepath.Dir(h.path), 0o700); err != nil {
		return
	}
	f, err := os.OpenFile(h.path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return
	}
	defer f.Close()
	line, _ := json.Marshal(histEntry{TS: time.Now(), Dir: dir, Origin: origin, Text: text, Blob: blob})
	_, _ = f.Write(append(line, '\n'))
}

func (h *History) search(term string) ([]histEntry, error) {
	f, err := os.Open(h.path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	defer f.Close()
	var out []histEntry
	term = strings.ToLower(term)
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	for sc.Scan() {
		var e histEntry
		if json.Unmarshal(sc.Bytes(), &e) != nil {
			continue
		}
		if term == "" || strings.Contains(strings.ToLower(e.Text), term) {
			out = append(out, e)
		}
	}
	return out, sc.Err()
}
