package httpapi

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"net/http"
	"strings"
	"time"

	"github.com/syaro/copysync/internal/auth"
	"github.com/syaro/copysync/internal/config"
	"github.com/syaro/copysync/internal/model"
	"golang.org/x/crypto/bcrypt"
)

func (s *Server) handleLogin(w http.ResponseWriter, r *http.Request) {
	if !s.loginLimiter.allow(r) {
		writeJSONError(w, http.StatusTooManyRequests, "rate_limited", "too many attempts, slow down")
		return
	}
	var req struct {
		Username string `json:"username"`
		Password string `json:"password"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "invalid body")
		return
	}
	admin, found, err := s.store.GetAdmin()
	if err != nil || !found {
		writeJSONError(w, http.StatusUnauthorized, "unauthorized", "invalid credentials")
		return
	}
	if !strings.EqualFold(req.Username, admin.Username) ||
		bcrypt.CompareHashAndPassword(admin.PassHash, []byte(req.Password)) != nil {
		writeJSONError(w, http.StatusUnauthorized, "unauthorized", "invalid credentials")
		return
	}
	if err := s.issueSession(w, admin.Username); err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "could not create session")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"username":     admin.Username,
		"mustChangePw": admin.MustChangePW,
	})
}

func (s *Server) handleLogout(w http.ResponseWriter, r *http.Request) {
	s.clearSession(w, r)
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
}

func (s *Server) handleMe(w http.ResponseWriter, r *http.Request) {
	admin, found, _ := s.store.GetAdmin()
	if !found {
		writeJSONError(w, http.StatusUnauthorized, "unauthorized", "no admin account")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"username":     admin.Username,
		"mustChangePw": admin.MustChangePW,
		"serverName":   s.serverName,
		"serverId":     s.serverID,
		"spkiPin":      s.spkiPin,
	})
}

func (s *Server) handlePassword(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Current string `json:"current"`
		New     string `json:"new"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "invalid body")
		return
	}
	if len(req.New) < 8 {
		writeJSONError(w, http.StatusBadRequest, "weak_password", "new password must be at least 8 characters")
		return
	}
	admin, found, err := s.store.GetAdmin()
	if err != nil || !found {
		writeJSONError(w, http.StatusInternalServerError, "internal", "no admin account")
		return
	}
	if bcrypt.CompareHashAndPassword(admin.PassHash, []byte(req.Current)) != nil {
		writeJSONError(w, http.StatusUnauthorized, "unauthorized", "current password is incorrect")
		return
	}
	hash, err := bcrypt.GenerateFromPassword([]byte(req.New), bcrypt.DefaultCost)
	if err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "could not hash password")
		return
	}
	admin.PassHash = hash
	admin.MustChangePW = false
	admin.UpdatedAt = s.now()
	if err := s.store.PutAdmin(admin); err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "could not save")
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
}

func (s *Server) handleListDevices(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{"devices": s.hub.Snapshot()})
}

func (s *Server) handleDeleteDevice(w http.ResponseWriter, r *http.Request) {
	id := model.DeviceID(r.PathValue("id"))
	if id == "" {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "missing device id")
		return
	}
	if err := s.store.DeleteDevice(id); err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "could not delete device")
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
}

func (s *Server) handleGetSettings(w http.ResponseWriter, r *http.Request) {
	settings, err := s.store.GetSettings()
	if err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "could not read settings")
		return
	}
	writeJSON(w, http.StatusOK, settings)
}

func (s *Server) handlePutSettings(w http.ResponseWriter, r *http.Request) {
	var rs config.RuntimeSettings
	if err := json.NewDecoder(r.Body).Decode(&rs); err != nil {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "invalid body")
		return
	}
	if err := s.store.PutSettings(rs); err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "could not save settings")
		return
	}
	out, _ := s.store.GetSettings()
	writeJSON(w, http.StatusOK, out)
}

// handleBroadcast pushes a text clip to every device from the server itself.
// It is only available when E2E is off (the server must be able to read the
// payload it broadcasts).
func (s *Server) handleBroadcast(w http.ResponseWriter, r *http.Request) {
	settings, _ := s.store.GetSettings()
	if settings.E2EEnabled || !settings.AllowServerBroadcast {
		writeJSONError(w, http.StatusForbidden, "broadcast_disabled", "server broadcast is disabled (E2E on or not allowed)")
		return
	}
	var req struct {
		Text string `json:"text"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil || req.Text == "" {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "text is required")
		return
	}
	sum := sha256.Sum256([]byte(req.Text))
	res := s.hub.Route(model.ClipEvent{
		ID:           auth.NewID("bcast"),
		OriginDevice: "server",
		TS:           s.now().Format(time.RFC3339),
		Mime:         []string{"text/plain"},
		InlineText:   req.Text,
		Size:         int64(len(req.Text)),
		Sha256:       hex.EncodeToString(sum[:]),
		Targets:      model.Targets{All: true},
	})
	writeJSON(w, http.StatusOK, map[string]any{"status": res.Status, "queuedFor": res.QueuedFor})
}
