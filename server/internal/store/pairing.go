package store

import (
	"encoding/json"
	"errors"
	"strings"
	"time"

	"github.com/syaro/copysync/internal/model"
	bolt "go.etcd.io/bbolt"
)

// ErrPairingInvalid is returned when an OTP is missing, expired, or already used.
var ErrPairingInvalid = errors.New("pairing code invalid, expired, or already used")

// ErrNameTaken is returned when a requested device name is already in use.
var ErrNameTaken = errors.New("device name already in use")

// PutPairing stores a new pairing code.
func (s *Store) PutPairing(p model.PairingCode) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		return putJSON(tx.Bucket(bucketPairing), []byte(p.Code), p)
	})
}

// ClaimPairing atomically validates the OTP, enforces device-name uniqueness,
// persists the device + token, and consumes the OTP — all in one transaction.
// It returns ErrPairingInvalid or ErrNameTaken on failure, leaving the database
// unchanged.
func (s *Store) ClaimPairing(code string, now time.Time, dev model.Device, tok model.TokenRecord) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		pb := tx.Bucket(bucketPairing)
		var p model.PairingCode
		found, err := getJSON(pb, []byte(code), &p)
		if err != nil {
			return err
		}
		if !found || p.Consumed() || p.Expired(now) {
			return ErrPairingInvalid
		}
		names := tx.Bucket(bucketDeviceNames)
		if existing := names.Get([]byte(strings.ToLower(dev.Name))); existing != nil && model.DeviceID(existing) != dev.ID {
			return ErrNameTaken
		}
		if err := putJSON(tx.Bucket(bucketDevices), []byte(dev.ID), dev); err != nil {
			return err
		}
		if err := names.Put([]byte(strings.ToLower(dev.Name)), []byte(dev.ID)); err != nil {
			return err
		}
		if err := putJSON(tx.Bucket(bucketTokens), []byte(tok.DeviceID), tok); err != nil {
			return err
		}
		if err := tx.Bucket(bucketTokenIndex).Put([]byte(tok.TokenHash), []byte(tok.DeviceID)); err != nil {
			return err
		}
		p.ConsumedAt = &now
		return putJSON(pb, []byte(code), p)
	})
}

// PurgePairing removes expired or consumed codes (periodic housekeeping).
func (s *Store) PurgePairing(now time.Time) error {
	return s.db.Update(func(tx *bolt.Tx) error {
		b := tx.Bucket(bucketPairing)
		var stale [][]byte
		err := b.ForEach(func(k, v []byte) error {
			var p model.PairingCode
			if json.Unmarshal(v, &p) == nil && (p.Consumed() || p.Expired(now)) {
				stale = append(stale, append([]byte(nil), k...))
			}
			return nil
		})
		if err != nil {
			return err
		}
		for _, k := range stale {
			_ = b.Delete(k)
		}
		return nil
	})
}
