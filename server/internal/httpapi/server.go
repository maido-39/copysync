// Package httpapi assembles the single HTTPS listener that serves the admin API,
// the device-pairing endpoints, the WebSocket control channel, the blob channel,
// and the embedded admin SPA.
package httpapi

import (
	"crypto/tls"
	"log/slog"
	"net/http"
	"time"

	"github.com/syaro/copysync/internal/blob"
	"github.com/syaro/copysync/internal/hub"
	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/store"
	"golang.org/x/time/rate"
)

// Server holds the HTTP API dependencies and builds the router.
type Server struct {
	store *store.Store
	hub   *hub.Hub
	log   *slog.Logger
	now   func() time.Time

	serverID   string
	serverName string
	spkiPin    string
	secret     string

	wsHandler http.Handler
	webui     http.Handler

	blobStore         *blob.FsBlobStore
	validateBlobToken func(string) (model.Device, bool)

	loginLimiter *ipLimiter
	pairLimiter  *ipLimiter

	blobWaiters *blobWaiters
}

// Config holds what the HTTP server needs at construction time.
type Config struct {
	Store             *store.Store
	Hub               *hub.Hub
	Log               *slog.Logger
	Now               func() time.Time
	ServerID          string
	ServerName        string
	SPKIPin           string
	Secret            string
	WSHandler         http.Handler
	WebUI             http.Handler
	BlobStore         *blob.FsBlobStore
	ValidateBlobToken func(string) (model.Device, bool)
}

// New creates the HTTP API server.
func New(c Config) *Server {
	now := c.Now
	if now == nil {
		now = time.Now
	}
	return &Server{
		store:             c.Store,
		hub:               c.Hub,
		log:               c.Log,
		now:               now,
		serverID:          c.ServerID,
		serverName:        c.ServerName,
		spkiPin:           c.SPKIPin,
		secret:            c.Secret,
		wsHandler:         c.WSHandler,
		webui:             c.WebUI,
		blobStore:         c.BlobStore,
		validateBlobToken: c.ValidateBlobToken,
		loginLimiter:      newIPLimiter(rate.Every(2*time.Second), 5),
		pairLimiter:       newIPLimiter(rate.Every(2*time.Second), 5),
		blobWaiters:       newBlobWaiters(),
	}
}

// Handler builds the root http.Handler.
func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()

	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		_, _ = w.Write([]byte("ok"))
	})

	// Real-time control channel + content-addressed blob channel.
	mux.Handle("GET /ws", s.wsHandler)
	mux.HandleFunc("PUT /blob/{id}", s.handleBlobPut)
	mux.HandleFunc("GET /blob/{id}", s.handleBlobGet)
	mux.HandleFunc("HEAD /blob/{id}", s.handleBlobGet)

	// Device pairing (public, rate-limited).
	mux.HandleFunc("GET /pair/serverinfo", s.handleServerInfo)
	mux.HandleFunc("POST /pair/claim", s.handlePairClaim)

	// Admin authentication.
	mux.HandleFunc("POST /admin/login", s.handleLogin)
	mux.HandleFunc("POST /admin/logout", s.requireSession(s.handleLogout))
	mux.HandleFunc("POST /admin/password", s.requireSession(s.handlePassword))
	mux.HandleFunc("GET /admin/me", s.requireSession(s.handleMe))

	// Admin resources (blocked until the first-run password change is done).
	mux.HandleFunc("GET /admin/devices", s.requireAdmin(s.handleListDevices))
	mux.HandleFunc("DELETE /admin/devices/{id}", s.requireAdmin(s.handleDeleteDevice))
	mux.HandleFunc("POST /admin/pairing", s.requireAdmin(s.handleCreatePairing))
	mux.HandleFunc("GET /admin/settings", s.requireAdmin(s.handleGetSettings))
	mux.HandleFunc("PUT /admin/settings", s.requireAdmin(s.handlePutSettings))
	mux.HandleFunc("POST /admin/broadcast", s.requireAdmin(s.handleBroadcast))

	// Admin SPA + static assets (catch-all GET).
	mux.Handle("GET /", s.webui)

	return s.recoverer(securityHeaders(mux))
}

// TLSConfig builds a tls.Config from the server's self-signed certificate.
func TLSConfig(cert tls.Certificate) *tls.Config {
	return &tls.Config{
		Certificates: []tls.Certificate{cert},
		MinVersion:   tls.VersionTLS12,
		NextProtos:   []string{"http/1.1"},
	}
}
