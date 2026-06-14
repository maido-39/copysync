package httpapi

import (
	"fmt"
	"html"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// safeName returns the cleaned base name, or "" if it is unsafe (path
// separators, "..", hidden, or empty) — so a download/delete can never escape
// the downloads directory.
func safeName(name string) string {
	name = filepath.Base(strings.TrimSpace(name))
	if name == "" || name == "." || name == ".." ||
		strings.HasPrefix(name, ".") || strings.ContainsAny(name, `/\`) {
		return ""
	}
	return name
}

func (s *Server) downloadsOn() bool {
	settings, err := s.store.GetSettings()
	return err == nil && settings.DownloadsEnabled
}

func humanSize(n int64) string {
	switch {
	case n >= 1<<30:
		return fmt.Sprintf("%.1f GiB", float64(n)/(1<<30))
	case n >= 1<<20:
		return fmt.Sprintf("%.1f MiB", float64(n)/(1<<20))
	case n >= 1<<10:
		return fmt.Sprintf("%.1f KiB", float64(n)/(1<<10))
	default:
		return fmt.Sprintf("%d B", n)
	}
}

// listFiles returns the regular, non-hidden files in the downloads dir.
func (s *Server) listFiles() []os.DirEntry {
	entries, _ := os.ReadDir(s.downloadsDir)
	out := make([]os.DirEntry, 0, len(entries))
	for _, e := range entries {
		if e.IsDir() || strings.HasPrefix(e.Name(), ".") {
			continue
		}
		out = append(out, e)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name() < out[j].Name() })
	return out
}

// GET /downloads/ — public index page (only when hosting is enabled).
func (s *Server) handleDownloadsIndex(w http.ResponseWriter, r *http.Request) {
	if !s.downloadsOn() {
		http.NotFound(w, r)
		return
	}
	var b strings.Builder
	b.WriteString(`<!doctype html><meta charset=utf-8>` +
		`<meta name=viewport content="width=device-width,initial-scale=1">` +
		`<title>CopySync 다운로드</title><style>` +
		`body{font:16px/1.7 system-ui,"Noto Sans KR",sans-serif;max-width:640px;margin:0 auto;padding:24px}` +
		`h1{font-size:20px}a{color:#2563eb;display:block;padding:6px 0}.muted{color:#888}</style>` +
		`<h1>CopySync 다운로드</h1>`)
	files := s.listFiles()
	if len(files) == 0 {
		b.WriteString(`<p class=muted>호스팅 중인 파일이 없습니다.</p>`)
	}
	for _, e := range files {
		info, _ := e.Info()
		var sz int64
		if info != nil {
			sz = info.Size()
		}
		nm := html.EscapeString(e.Name())
		b.WriteString(`<a href="` + nm + `">` + nm + ` <span class=muted>(` + humanSize(sz) + `)</span></a>`)
	}
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	_, _ = w.Write([]byte(b.String()))
}

// GET /downloads/{name} — serve a hosted file (only when enabled).
func (s *Server) handleDownloadFile(w http.ResponseWriter, r *http.Request) {
	if !s.downloadsOn() {
		http.NotFound(w, r)
		return
	}
	name := safeName(r.PathValue("name"))
	if name == "" {
		http.NotFound(w, r)
		return
	}
	p := filepath.Join(s.downloadsDir, name)
	if fi, err := os.Stat(p); err != nil || fi.IsDir() {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Content-Disposition", `attachment; filename="`+name+`"`)
	http.ServeFile(w, r, p)
}

// GET /admin/downloads — list hosted files (admin).
func (s *Server) handleAdminDownloadsList(w http.ResponseWriter, r *http.Request) {
	type item struct {
		Name string `json:"name"`
		Size int64  `json:"size"`
	}
	files := []item{}
	for _, e := range s.listFiles() {
		info, _ := e.Info()
		var sz int64
		if info != nil {
			sz = info.Size()
		}
		files = append(files, item{Name: e.Name(), Size: sz})
	}
	writeJSON(w, http.StatusOK, map[string]any{"files": files})
}

// POST /admin/downloads — upload a file (multipart field "file") (admin).
func (s *Server) handleAdminDownloadUpload(w http.ResponseWriter, r *http.Request) {
	if err := r.ParseMultipartForm(32 << 20); err != nil {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "invalid upload")
		return
	}
	file, hdr, err := r.FormFile("file")
	if err != nil {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "no file")
		return
	}
	defer file.Close()
	name := safeName(hdr.Filename)
	if name == "" {
		writeJSONError(w, http.StatusBadRequest, "bad_name", "unsafe file name")
		return
	}
	if err := os.MkdirAll(s.downloadsDir, 0o700); err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "mkdir failed")
		return
	}
	dst, err := os.Create(filepath.Join(s.downloadsDir, name))
	if err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "create failed")
		return
	}
	defer dst.Close()
	if _, err := io.Copy(dst, file); err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "write failed")
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok", "name": name})
}

// DELETE /admin/downloads/{name} — delete a hosted file (admin).
func (s *Server) handleAdminDownloadDelete(w http.ResponseWriter, r *http.Request) {
	name := safeName(r.PathValue("name"))
	if name == "" {
		writeJSONError(w, http.StatusBadRequest, "bad_name", "unsafe name")
		return
	}
	_ = os.Remove(filepath.Join(s.downloadsDir, name))
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
}
