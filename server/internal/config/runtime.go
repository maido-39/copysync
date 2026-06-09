package config

// RuntimeSettings are admin-settable knobs persisted in the store and editable
// from the admin UI without restarting the server.
type RuntimeSettings struct {
	MaxMessageBytes        int64    `json:"maxMessageBytes"`        // WS read limit; inline text must fit
	BlobMaxBytes           int64    `json:"blobMaxBytes"`           // per-blob cap on PUT
	OnDemandThresholdBytes int64    `json:"onDemandThresholdBytes"` // files ≤ this upload eagerly; larger pull on demand
	BlobStoreCapBytes      int64    `json:"blobStoreCapBytes"`      // total blob dir cap (LRU GC)
	QueueDepthPerDevice    int      `json:"queueDepthPerDevice"`    // max queued items per offline device
	QueueItemTTLSeconds    int64    `json:"queueItemTtlSeconds"`    // drop stale queued items after
	BlobTTLSeconds         int64    `json:"blobTtlSeconds"`         // delete unreferenced blobs after
	E2EEnabled             bool     `json:"e2eEnabled"`             // when true, server never sees plaintext
	AllowServerBroadcast   bool     `json:"allowServerBroadcast"`   // admin broadcast (auto-off when E2E on)
	SessionTTLSeconds      int64    `json:"sessionTtlSeconds"`      // admin session lifetime
	PairingCodeTTLSeconds  int64    `json:"pairingCodeTtlSeconds"`
	Pools                  []string `json:"pools"` // available share pools; clips route within a pool
}

// DefaultRuntimeSettings returns the built-in defaults.
func DefaultRuntimeSettings() RuntimeSettings {
	return RuntimeSettings{
		MaxMessageBytes:        64 * 1024,              // 64 KiB
		BlobMaxBytes:           100 * 1024 * 1024,      // 100 MiB
		OnDemandThresholdBytes: 5 * 1024 * 1024,        // 5 MiB
		BlobStoreCapBytes:      5 * 1024 * 1024 * 1024, // 5 GiB
		QueueDepthPerDevice:    200,
		QueueItemTTLSeconds:    72 * 3600,
		BlobTTLSeconds:         72 * 3600,
		E2EEnabled:             false,
		AllowServerBroadcast:   true,
		SessionTTLSeconds:      12 * 3600,
		PairingCodeTTLSeconds:  5 * 60,
		Pools:                  []string{"default"},
	}
}

// Normalize enforces invariants: E2E forbids server-readable broadcast, and a
// few values have hard floors.
func (s *RuntimeSettings) Normalize() {
	if s.E2EEnabled {
		s.AllowServerBroadcast = false
	}
	if s.MaxMessageBytes < 1024 {
		s.MaxMessageBytes = 1024
	}
	if s.BlobMaxBytes < 1024 {
		s.BlobMaxBytes = 1024
	}
	if s.OnDemandThresholdBytes < 0 {
		s.OnDemandThresholdBytes = 0
	}
	if s.QueueDepthPerDevice < 1 {
		s.QueueDepthPerDevice = 1
	}
	if s.SessionTTLSeconds < 60 {
		s.SessionTTLSeconds = 60
	}
	if s.PairingCodeTTLSeconds < 30 {
		s.PairingCodeTTLSeconds = 30
	}
	// Pools: always include "default", dedupe, drop blanks.
	seen := map[string]bool{}
	out := make([]string, 0, len(s.Pools)+1)
	for _, p := range append([]string{"default"}, s.Pools...) {
		if p != "" && !seen[p] {
			seen[p] = true
			out = append(out, p)
		}
	}
	s.Pools = out
}
