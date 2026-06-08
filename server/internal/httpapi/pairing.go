package httpapi

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"net"
	"net/http"
	"time"

	qrcode "github.com/skip2/go-qrcode"
	"github.com/syaro/copysync/internal/auth"
	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/protocol"
	"github.com/syaro/copysync/internal/store"
)

// PairPayload is everything a new device needs to connect. It is shown as a QR
// code in the admin UI and can also be entered manually.
type PairPayload struct {
	ServerID   string `json:"serverId"`
	ServerName string `json:"serverName"`
	Host       string `json:"host"`
	Port       string `json:"port"`
	SPKIPin    string `json:"spkiPin"`
	OTP        string `json:"otp"`
}

// handleServerInfo (public) lets a device confirm the server identity and pin.
func (s *Server) handleServerInfo(w http.ResponseWriter, r *http.Request) {
	if !s.pairLimiter.allow(r) {
		writeJSONError(w, http.StatusTooManyRequests, "rate_limited", "slow down")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"serverId":   s.serverID,
		"serverName": s.serverName,
		"spkiPin":    s.spkiPin,
		"proto":      protocol.Proto,
	})
}

// handleCreatePairing (admin) generates a single-use OTP and a QR payload.
func (s *Server) handleCreatePairing(w http.ResponseWriter, r *http.Request) {
	settings, _ := s.store.GetSettings()
	now := s.now()
	code := auth.NewOTP(8)
	pc := model.PairingCode{
		Code:      code,
		CreatedAt: now,
		ExpiresAt: now.Add(time.Duration(settings.PairingCodeTTLSeconds) * time.Second),
	}
	if err := s.store.PutPairing(pc); err != nil {
		writeJSONError(w, http.StatusInternalServerError, "internal", "could not create pairing code")
		return
	}
	host, port := splitHostPortDefault(r.Host, "8443")
	payload := PairPayload{
		ServerID:   s.serverID,
		ServerName: s.serverName,
		Host:       host,
		Port:       port,
		SPKIPin:    s.spkiPin,
		OTP:        code,
	}
	payloadJSON, _ := json.Marshal(payload)
	qrDataURI := ""
	if png, err := qrcode.Encode(string(payloadJSON), qrcode.Medium, 320); err == nil {
		qrDataURI = "data:image/png;base64," + base64.StdEncoding.EncodeToString(png)
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"otp":       code,
		"expiresAt": pc.ExpiresAt,
		"payload":   payload,
		"qr":        qrDataURI,
	})
}

// handlePairClaim (public) redeems an OTP and issues a device token.
func (s *Server) handlePairClaim(w http.ResponseWriter, r *http.Request) {
	if !s.pairLimiter.allow(r) {
		writeJSONError(w, http.StatusTooManyRequests, "rate_limited", "slow down")
		return
	}
	var req struct {
		OTP        string         `json:"otp"`
		DeviceName string         `json:"deviceName"`
		Platform   model.Platform `json:"platform"`
		PubKey     string         `json:"pubkey,omitempty"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "invalid body")
		return
	}
	if req.OTP == "" || req.DeviceName == "" {
		writeJSONError(w, http.StatusBadRequest, "bad_request", "otp and deviceName are required")
		return
	}
	now := s.now()
	deviceID := model.DeviceID(auth.NewID("dev"))
	token := auth.GenerateToken()
	dev := model.Device{
		ID:         deviceID,
		Name:       req.DeviceName,
		Platform:   req.Platform,
		CreatedAt:  now,
		LastSeenAt: now,
	}
	tok := model.TokenRecord{
		DeviceID:  deviceID,
		TokenHash: auth.HashToken(s.secret, token),
		IssuedAt:  now,
	}
	if err := s.store.ClaimPairing(req.OTP, now, dev, tok); err != nil {
		switch {
		case errors.Is(err, store.ErrPairingInvalid):
			writeJSONError(w, http.StatusGone, "pairing_invalid", "code is invalid, expired, or already used")
		case errors.Is(err, store.ErrNameTaken):
			writeJSONError(w, http.StatusConflict, "name_taken", "device name already in use")
		default:
			writeJSONError(w, http.StatusInternalServerError, "internal", "pairing failed")
		}
		return
	}
	settings, _ := s.store.GetSettings()
	writeJSON(w, http.StatusOK, map[string]any{
		"deviceId":   deviceID,
		"token":      token, // returned exactly once; only the HMAC is stored
		"serverId":   s.serverID,
		"serverName": s.serverName,
		"e2e":        settings.E2EEnabled,
	})
}

func splitHostPortDefault(hostport, defPort string) (host, port string) {
	h, p, err := net.SplitHostPort(hostport)
	if err != nil {
		return hostport, defPort
	}
	return h, p
}
