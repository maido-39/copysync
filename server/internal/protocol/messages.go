// Package protocol defines the WebSocket control-channel wire format shared by
// the server and every client. The canonical, human-readable description lives
// in docs/PROTOCOL.md; this file is the Go source of truth.
package protocol

import (
	"encoding/json"

	"github.com/syaro/copysync/internal/model"
)

// Proto is the protocol version this server speaks.
const Proto = 1

// Message types — the "t" field of the envelope.
const (
	TypeHello    = "hello"     // C->S
	TypeHelloOK  = "hello_ok"  // S->C
	TypeHelloErr = "hello_err" // S->C
	TypeClip     = "clip"      // both directions
	TypeAck      = "ack"       // S->C
	TypePresence = "presence"  // S->C
	TypeRoster   = "roster"    // S->C
	TypeError    = "error"     // S->C
)

// Envelope wraps every control-channel frame: {"t": <type>, "d": <payload>}.
type Envelope struct {
	T string          `json:"t"`
	D json.RawMessage `json:"d"`
}

// DeviceInfo is a device plus its current online status, used in rosters.
type DeviceInfo struct {
	model.Device
	Online bool `json:"online"`
}

// Hello is the client's first frame after connecting.
type Hello struct {
	DeviceID   model.DeviceID `json:"deviceId"`
	DeviceName string         `json:"deviceName"`
	Token      string         `json:"token"`
	Platform   model.Platform `json:"platform"`
	Proto      int            `json:"proto"`
}

// HelloOK is the server's acceptance of a Hello.
type HelloOK struct {
	ServerID   string       `json:"serverId"`
	ServerName string       `json:"serverName"`
	E2E        bool         `json:"e2e"`
	You        model.Device `json:"you"`
	Roster     []DeviceInfo `json:"roster"`
	MaxMsg     int64        `json:"maxMsg"`
	BlobCap    int64        `json:"blobCap"`
}

// HelloErr explains why a Hello was rejected (the connection then closes).
type HelloErr struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

// Ack reports the disposition of a clip the client sent.
type Ack struct {
	ID        string           `json:"id"`
	Status    string           `json:"status"` // relayed | queued | rejected
	QueuedFor []model.DeviceID `json:"queuedFor,omitempty"`
	Message   string           `json:"message,omitempty"`
}

// Ack status values.
const (
	AckRelayed  = "relayed"
	AckQueued   = "queued"
	AckRejected = "rejected"
)

// Presence is a roster delta for a single device.
type Presence struct {
	Device model.Device `json:"device"`
	Online bool         `json:"online"`
}

// Roster is the full set of paired devices with their online flags.
type Roster struct {
	Devices []DeviceInfo `json:"devices"`
}

// Error is a non-fatal server error notification.
type Error struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}
