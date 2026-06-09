package main

import (
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"errors"

	"golang.org/x/crypto/argon2"
)

// e2eAlg is the AEAD label carried in ClipEvent.Enc.Alg. AES-256-GCM is used
// because it is native on both the Go stdlib and the Android JDK, so the Go and
// Kotlin clients interoperate without a third-party AEAD library.
const e2eAlg = "aes-256-gcm"

// deriveKey turns a user passphrase into the 32-byte group key. The salt is
// derived from the server id so every device on the same server derives the
// same key — while the server, which never sees the passphrase, cannot.
func deriveKey(pass, serverID string) []byte {
	salt := sha256.Sum256([]byte("copysync-e2e|" + serverID))
	return argon2.IDKey([]byte(pass), salt[:], 1, 64*1024, 4, 32)
}

// keyID is a short non-secret fingerprint so receivers can detect a passphrase
// mismatch without trying to decrypt.
func keyID(key []byte) string {
	s := sha256.Sum256(key)
	return hex.EncodeToString(s[:])[:16]
}

func gcmOf(key []byte) (cipher.AEAD, error) {
	block, err := aes.NewCipher(key)
	if err != nil {
		return nil, err
	}
	return cipher.NewGCM(block) // 12-byte nonce, 16-byte tag
}

// seal returns nonce || AES-GCM(ciphertext+tag) — a self-describing blob that
// open reverses with the same key.
func seal(key, plaintext []byte) ([]byte, error) {
	aead, err := gcmOf(key)
	if err != nil {
		return nil, err
	}
	nonce := make([]byte, aead.NonceSize())
	if _, err := rand.Read(nonce); err != nil {
		return nil, err
	}
	return aead.Seal(nonce, nonce, plaintext, nil), nil
}

func open(key, raw []byte) ([]byte, error) {
	aead, err := gcmOf(key)
	if err != nil {
		return nil, err
	}
	if len(raw) < aead.NonceSize() {
		return nil, errors.New("ciphertext too short")
	}
	nonce, ct := raw[:aead.NonceSize()], raw[aead.NonceSize():]
	return aead.Open(nil, nonce, ct, nil)
}
