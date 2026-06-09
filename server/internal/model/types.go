// Package model holds the pure data types shared across the CopySync server.
// It deliberately has no dependencies on other internal packages so that every
// layer can import it without creating cycles.
package model

import (
	"encoding/json"
	"errors"
	"time"
)

// DeviceID is a stable, server-assigned identifier for a paired client.
type DeviceID string

// BlobID is the content address of a blob: "sha256:" + hex digest.
type BlobID string

// Platform identifies the client OS (informational only).
type Platform string

// Device is a paired client. The bearer token is never stored on the device
// record; its HMAC lives in a separate TokenRecord.
type Device struct {
	ID         DeviceID  `json:"id"`
	Name       string    `json:"name"`
	Platform   Platform  `json:"platform"`
	CreatedAt  time.Time `json:"createdAt"`
	LastSeenAt time.Time `json:"lastSeenAt"`
	Revoked    bool      `json:"revoked"`
}

// TokenRecord stores the keyed hash of a device's long-lived bearer token.
// The plaintext token is returned to the client exactly once, at pairing time.
type TokenRecord struct {
	DeviceID  DeviceID  `json:"deviceId"`
	TokenHash string    `json:"tokenHash"` // base64(HMAC-SHA256(token, serverSecret))
	IssuedAt  time.Time `json:"issuedAt"`
	Revoked   bool      `json:"revoked"`
}

// PairingCode is a single-use OTP that bootstraps a device pairing.
type PairingCode struct {
	Code       string     `json:"code"`
	CreatedAt  time.Time  `json:"createdAt"`
	ExpiresAt  time.Time  `json:"expiresAt"`
	ConsumedAt *time.Time `json:"consumedAt,omitempty"`
}

// Expired reports whether the code is past its expiry.
func (p PairingCode) Expired(now time.Time) bool { return now.After(p.ExpiresAt) }

// Consumed reports whether the code has already been redeemed.
func (p PairingCode) Consumed() bool { return p.ConsumedAt != nil }

// EncMeta marks a clip payload as end-to-end encrypted (Stage 3; unused while
// E2E is off — the server then sees plaintext metadata and inline text).
type EncMeta struct {
	Alg   string `json:"alg"`   // e.g. "xchacha20poly1305"
	KeyID string `json:"keyId"` // device-group key id (server stores only wrapped copies)
	Nonce string `json:"nonce"` // base64-encoded 24-byte nonce
}

// Targets selects the recipients of a clip: either all paired devices, or an
// explicit list. Its JSON form is the string "all" or an array of device ids.
type Targets struct {
	All     bool
	Devices []DeviceID
}

// MarshalJSON encodes Targets as "all" or a device-id array (never null, so the
// value round-trips unambiguously).
func (t Targets) MarshalJSON() ([]byte, error) {
	if t.All {
		return json.Marshal("all")
	}
	if t.Devices == nil {
		return []byte("[]"), nil
	}
	return json.Marshal(t.Devices)
}

// UnmarshalJSON decodes null (no targets), the string "all", or a device-id array.
func (t *Targets) UnmarshalJSON(b []byte) error {
	if string(b) == "null" {
		t.All = false
		t.Devices = nil
		return nil
	}
	var s string
	if err := json.Unmarshal(b, &s); err == nil {
		if s == "all" {
			t.All = true
			t.Devices = nil
			return nil
		}
		return errors.New("targets: unknown string value " + s)
	}
	var ids []DeviceID
	if err := json.Unmarshal(b, &ids); err != nil {
		return errors.New("targets: must be \"all\" or an array of device ids")
	}
	t.All = false
	t.Devices = ids
	return nil
}

// ClipEvent is a clipboard item relayed through the server. Large payloads are
// not inlined; InlineText carries only small text (and only while E2E is off),
// otherwise BlobID references the payload on the HTTPS blob channel.
type ClipEvent struct {
	ID           string   `json:"id"`
	OriginDevice DeviceID `json:"originDeviceId"`
	Seq          uint64   `json:"seq"`
	TS           string   `json:"ts"` // RFC3339; relayed as-is, server stamps when empty
	Mime         []string `json:"mime"`
	InlineText   string   `json:"inlineText,omitempty"`
	Html         string   `json:"html,omitempty"` // rich-text (text/html) variant; encrypted like InlineText when E2E
	BlobID       BlobID   `json:"blobId,omitempty"`
	Name         string   `json:"name,omitempty"`     // filename hint for file payloads
	OnDemand     bool     `json:"onDemand,omitempty"` // bytes not uploaded yet; pull from origin on request
	Size         int64    `json:"size"`
	Sha256       string   `json:"sha256"`
	Targets      Targets  `json:"targets"`
	Enc          *EncMeta `json:"enc,omitempty"`
}

// QueueItem is a clip held for a device that was offline when it was sent.
type QueueItem struct {
	Event      ClipEvent `json:"event"`
	EnqueuedAt time.Time `json:"enqueuedAt"`
}

// BlobEntry is on-disk blob metadata; the bytes themselves live on the filesystem.
type BlobEntry struct {
	ID         BlobID    `json:"id"`
	Size       int64     `json:"size"`
	Mime       string    `json:"mime"`
	CreatedAt  time.Time `json:"createdAt"`
	LastAccess time.Time `json:"lastAccess"`
	Refcount   int       `json:"refcount"`
}

// AdminUser is the single administrator account for the web UI.
type AdminUser struct {
	Username     string    `json:"username"`
	PassHash     []byte    `json:"passHash"` // bcrypt
	MustChangePW bool      `json:"mustChangePw"`
	UpdatedAt    time.Time `json:"updatedAt"`
}

// Session is a logged-in admin session. The cookie holds the raw id; the store
// keeps only its hash.
type Session struct {
	IDHash    string    `json:"idHash"` // base64(sha256(rawSessionID))
	Username  string    `json:"username"`
	CreatedAt time.Time `json:"createdAt"`
	ExpiresAt time.Time `json:"expiresAt"`
}
