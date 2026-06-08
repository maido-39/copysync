// Package webui serves the embedded admin single-page app over the admin HTTPS
// listener. The assets are plain HTML/CSS/JS with no build step, compiled into
// the binary via go:embed.
package webui

import (
	"embed"
	"io/fs"
	"net/http"
)

//go:embed dist
var distFS embed.FS

// Handler returns an http.Handler that serves the SPA assets, falling back to
// index.html for any path that is not a real file so client-side routing works.
func Handler() http.Handler {
	sub, err := fs.Sub(distFS, "dist")
	if err != nil {
		panic(err)
	}
	fileServer := http.FileServer(http.FS(sub))
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		clean := r.URL.Path
		if clean != "/" {
			name := clean
			if name[0] == '/' {
				name = name[1:]
			}
			if _, err := fs.Stat(sub, name); err == nil {
				fileServer.ServeHTTP(w, r)
				return
			}
		}
		data, err := fs.ReadFile(sub, "index.html")
		if err != nil {
			http.Error(w, "admin UI not built", http.StatusNotFound)
			return
		}
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		_, _ = w.Write(data)
	})
}
