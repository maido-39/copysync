package main

import (
	"encoding/json"
	"os"
	"path/filepath"
)

// Config is the persisted pairing state for a copyctl device.
type Config struct {
	ServerURL  string `json:"serverUrl"`
	ServerName string `json:"serverName"`
	ServerID   string `json:"serverId"` // used as the E2E key-derivation salt
	DeviceID   string `json:"deviceId"`
	DeviceName string `json:"deviceName"`
	Token      string `json:"token"`   // bearer token (secret)
	Pin        string `json:"spkiPin"` // server SPKI SHA-256, base64
	E2EPass    string `json:"e2ePass"` // optional E2E passphrase (secret); empty = no E2E
	// Privacy filter (mirrors the desktop/Android clients): captured clips that
	// classify as sensitive (passwords/OTP/cards/keys/custom) are NOT synced.
	// Zero value = filter ON, so configs from before this field keep the safe
	// default; explicit `send --text` is never blocked (deliberate user action).
	PrivacyFilterOff bool     `json:"privacyFilterOff,omitempty"`
	CustomPatterns   []string `json:"customPatterns,omitempty"` // extra sensitive regexes
}

func defaultConfigPath() string {
	if dir, err := os.UserConfigDir(); err == nil && dir != "" {
		return filepath.Join(dir, "copysync", "cli.json")
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".config", "copysync", "cli.json")
}

func loadConfig(path string) (Config, error) {
	var c Config
	data, err := os.ReadFile(path)
	if err != nil {
		return c, err
	}
	return c, json.Unmarshal(data, &c)
}

func saveConfig(path string, c Config) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0o600)
}

func defaultHistoryPath() string {
	if dir, err := os.UserConfigDir(); err == nil && dir != "" {
		return filepath.Join(dir, "copysync", "history.jsonl")
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".config", "copysync", "history.jsonl")
}
