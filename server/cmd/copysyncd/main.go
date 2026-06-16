// Command copysyncd is the CopySync relay server: a single self-contained binary
// that serves the admin UI, device pairing, the WebSocket control channel, and
// (Pass B) the blob channel over one self-signed HTTPS listener.
package main

import (
	"context"
	"errors"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/syaro/copysync/internal/auth"
	"github.com/syaro/copysync/internal/blob"
	"github.com/syaro/copysync/internal/config"
	"github.com/syaro/copysync/internal/httpapi"
	"github.com/syaro/copysync/internal/hub"
	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/store"
	"github.com/syaro/copysync/internal/tlsgen"
	"github.com/syaro/copysync/internal/transport"
	"github.com/syaro/copysync/internal/webui"
	"golang.org/x/crypto/bcrypt"
)

func main() {
	cfg := config.Load()
	log := newLogger(cfg.LogLevel)
	if err := run(cfg, log); err != nil {
		log.Error("server exited with error", "err", err)
		os.Exit(1)
	}
}

func run(cfg config.Config, log *slog.Logger) error {
	if err := os.MkdirAll(cfg.DataDir, 0o700); err != nil {
		return err
	}
	st, err := store.Open(cfg.DataDir)
	if err != nil {
		return err
	}
	defer st.Close()

	// Stable server identity + the HMAC secret used to hash device tokens.
	serverID, err := st.GetOrCreateMeta("serverId", func() string { return auth.NewID("srv") })
	if err != nil {
		return err
	}
	secret, err := st.GetOrCreateMeta("serverSecret", auth.NewSecret)
	if err != nil {
		return err
	}

	tlsRes, err := tlsgen.LoadOrCreate(cfg.DataDir, cfg.ServerName, cfg.TLSHosts)
	if err != nil {
		return err
	}

	blobStore, err := blob.NewFsBlobStore(cfg.DataDir)
	if err != nil {
		return err
	}

	if err := seedAdmin(st, cfg, log); err != nil {
		return err
	}

	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()

	h := hub.New(st, log, time.Now, serverID, cfg.ServerName)
	go h.Run(ctx)
	go housekeeping(ctx, st, blobStore, log)

	validate := func(id model.DeviceID, token string) (model.Device, bool) {
		dev, found, err := st.GetDevice(id)
		if err != nil || !found || dev.Revoked {
			return model.Device{}, false
		}
		rec, found, err := st.GetToken(id)
		if err != nil || !found || rec.Revoked {
			return model.Device{}, false
		}
		presented := auth.HashToken(secret, token)
		// Accept the current token, or the previous one during a rotation grace.
		if !auth.ConstantTimeEqual(rec.TokenHash, presented) && !auth.ConstantTimeEqual(rec.PrevHash, presented) {
			return model.Device{}, false
		}
		return dev, true
	}

	// maybeRotate runs after a successful auth (Stage-3 token rotation): it retires
	// the old token once the new one is in use, and re-issues tokens older than the
	// configured age. Returns a new plaintext token to deliver, or "".
	maybeRotate := func(id model.DeviceID, token string) string {
		rec, found, err := st.GetToken(id)
		if err != nil || !found {
			return ""
		}
		presented := auth.HashToken(secret, token)
		curMatch := auth.ConstantTimeEqual(rec.TokenHash, presented)
		// Client presented the NEW token while an old one is still pending → retire it.
		if curMatch && rec.PrevHash != "" {
			_ = st.RetireOldToken(id)
			return ""
		}
		// Re-issue only from the current token, when enabled, not already rotating, and old enough.
		s, _ := st.GetSettings()
		if s.TokenRotateDays <= 0 || rec.PrevHash != "" || !curMatch {
			return ""
		}
		if time.Since(rec.IssuedAt) < time.Duration(s.TokenRotateDays)*24*time.Hour {
			return ""
		}
		newToken := auth.GenerateToken()
		if st.RotateToken(id, auth.HashToken(secret, newToken), time.Now()) != nil {
			return ""
		}
		return newToken
	}

	// The blob channel authenticates by bearer token alone; resolve it to a
	// device via the token-hash index, then reuse the same validation.
	validateBlobToken := func(token string) (model.Device, bool) {
		id, found, err := st.DeviceIDByTokenHash(auth.HashToken(secret, token))
		if err != nil || !found {
			return model.Device{}, false
		}
		return validate(id, token)
	}

	wsHandler := transport.Handler(transport.Deps{
		Hub:              h,
		Log:              log,
		Now:              time.Now,
		ValidateToken:    validate,
		MaybeRotateToken: maybeRotate,
		MaxMessage:       func() int64 { s, _ := st.GetSettings(); return s.MaxMessageBytes },
	})

	api := httpapi.New(httpapi.Config{
		Store:             st,
		Hub:               h,
		Log:               log,
		ServerID:          serverID,
		ServerName:        cfg.ServerName,
		SPKIPin:           tlsRes.SPKIPin,
		Secret:            secret,
		APIKey:            cfg.APIKey,
		WSHandler:         wsHandler,
		WebUI:             webui.Handler(),
		BlobStore:         blobStore,
		ValidateBlobToken: validateBlobToken,
		DataDir:           cfg.DataDir,
	})

	srv := &http.Server{
		Addr:              cfg.HTTPSAddr,
		Handler:           api.Handler(),
		TLSConfig:         httpapi.TLSConfig(tlsRes.Certificate),
		ReadHeaderTimeout: 10 * time.Second,
	}

	startupBanner(log, cfg, serverID, tlsRes.SPKIPin)
	registerMDNS(ctx, cfg.ServerName, serverID, cfg.HTTPSAddr, log)

	errCh := make(chan error, 1)
	go func() {
		// Certificates come from TLSConfig, so the file arguments are empty.
		errCh <- srv.ListenAndServeTLS("", "")
	}()

	select {
	case <-ctx.Done():
		log.Info("shutdown signal received")
	case err := <-errCh:
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			return err
		}
	}

	shutdownCtx, cancelShutdown := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancelShutdown()
	return srv.Shutdown(shutdownCtx)
}

func seedAdmin(st *store.Store, cfg config.Config, log *slog.Logger) error {
	if _, found, err := st.GetAdmin(); err != nil {
		return err
	} else if found {
		return nil
	}
	hash, err := bcrypt.GenerateFromPassword([]byte(cfg.AdminPass), bcrypt.DefaultCost)
	if err != nil {
		return err
	}
	if err := st.PutAdmin(model.AdminUser{
		Username:     cfg.AdminUser,
		PassHash:     hash,
		MustChangePW: true,
		UpdatedAt:    time.Now(),
	}); err != nil {
		return err
	}
	log.Warn("seeded default admin account — you must change the password on first login",
		"username", cfg.AdminUser)
	return nil
}

func housekeeping(ctx context.Context, st *store.Store, blobStore *blob.FsBlobStore, log *slog.Logger) {
	ticker := time.NewTicker(10 * time.Minute)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			now := time.Now()
			if err := st.PurgeExpiredSessions(now); err != nil {
				log.Warn("purge sessions failed", "err", err)
			}
			if err := st.PurgePairing(now); err != nil {
				log.Warn("purge pairing failed", "err", err)
			}
			settings, _ := st.GetSettings()
			if n, err := st.PurgeExpiredQueueItems(now, time.Duration(settings.QueueItemTTLSeconds)*time.Second); err != nil {
				log.Warn("purge queue failed", "err", err)
			} else if n > 0 {
				log.Info("purged expired queue items", "count", n)
			}
			deleted, err := blob.RunGC(blobStore, st, blob.GCConfig{
				Now:      now,
				BlobTTL:  time.Duration(settings.BlobTTLSeconds) * time.Second,
				StoreCap: settings.BlobStoreCapBytes,
			})
			if err != nil {
				log.Warn("blob gc failed", "err", err)
			} else if deleted > 0 {
				log.Info("blob gc removed blobs", "count", deleted)
			}
		}
	}
}

func startupBanner(log *slog.Logger, cfg config.Config, serverID, pin string) {
	log.Info("CopySync server starting",
		"name", cfg.ServerName,
		"id", serverID,
		"addr", cfg.HTTPSAddr,
		"dataDir", cfg.DataDir,
		"spkiPin", pin,
	)
}

func newLogger(level string) *slog.Logger {
	var lvl slog.Level
	switch level {
	case "debug":
		lvl = slog.LevelDebug
	case "warn":
		lvl = slog.LevelWarn
	case "error":
		lvl = slog.LevelError
	default:
		lvl = slog.LevelInfo
	}
	return slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: lvl}))
}
