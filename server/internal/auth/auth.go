// Package auth provides identifier/token/OTP generation and the keyed hashing
// used to store bearer tokens and session ids without keeping their plaintext.
package auth

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base32"
	"encoding/base64"
	"encoding/hex"
	"fmt"
	"math/big"
	"strings"
)

// NewID returns a random identifier with the given prefix and 128 bits of
// entropy, e.g. NewID("dev") -> "dev_3f2a...".
func NewID(prefix string) string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	enc := base32.StdEncoding.WithPadding(base32.NoPadding)
	return prefix + "_" + strings.ToLower(enc.EncodeToString(b[:]))
}

// GenerateToken returns a 256-bit random bearer token (base64url, unpadded).
func GenerateToken() string {
	var b [32]byte
	_, _ = rand.Read(b[:])
	return base64.RawURLEncoding.EncodeToString(b[:])
}

// HashToken computes base64(HMAC-SHA256(token, secret)).
func HashToken(secret, token string) string {
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(token))
	return base64.StdEncoding.EncodeToString(mac.Sum(nil))
}

// ConstantTimeEqual compares two strings without leaking timing information.
func ConstantTimeEqual(a, b string) bool {
	return hmac.Equal([]byte(a), []byte(b))
}

// NewSecret returns a 256-bit random secret as hex (the server's HMAC key).
func NewSecret() string {
	var b [32]byte
	_, _ = rand.Read(b[:])
	return hex.EncodeToString(b[:])
}

// NewOTP returns a numeric one-time code with the given number of digits.
func NewOTP(digits int) string {
	if digits < 4 {
		digits = 6
	}
	max := big.NewInt(1)
	ten := big.NewInt(10)
	for i := 0; i < digits; i++ {
		max.Mul(max, ten)
	}
	n, _ := rand.Int(rand.Reader, max)
	return fmt.Sprintf("%0*d", digits, n.Int64())
}

// NewSessionID returns a random raw session id and the hash to store for it.
func NewSessionID() (raw, idHash string) {
	var b [32]byte
	_, _ = rand.Read(b[:])
	raw = base64.RawURLEncoding.EncodeToString(b[:])
	return raw, HashSessionID(raw)
}

// HashSessionID returns base64(sha256(raw)) — the value stored for a session.
func HashSessionID(raw string) string {
	sum := sha256.Sum256([]byte(raw))
	return base64.StdEncoding.EncodeToString(sum[:])
}
