// Package config holds boot-time configuration (read from environment variables)
// and the admin-settable runtime settings persisted in the store.
package config

import (
	"os"
	"strings"
)

// Config is the boot-time configuration, sourced from environment variables.
type Config struct {
	DataDir    string   // COPYSYNC_DATA_DIR
	HTTPSAddr  string   // COPYSYNC_HTTPS_ADDR
	ServerName string   // COPYSYNC_SERVER_NAME
	TLSHosts   []string // COPYSYNC_TLS_HOSTS (comma-separated extra SAN entries)
	AdminUser  string   // COPYSYNC_ADMIN_USER (seed only)
	AdminPass  string   // COPYSYNC_ADMIN_PASS (seed only; forces change on first login)
	APIKey     string   // COPYSYNC_API_KEY (programmatic admin auth; empty = disabled)
	LogLevel   string   // COPYSYNC_LOG_LEVEL
}

func getenv(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

// Load reads configuration from the environment, applying sane defaults.
func Load() Config {
	hostname, _ := os.Hostname()
	if hostname == "" {
		hostname = "copysync"
	}
	c := Config{
		DataDir:    getenv("COPYSYNC_DATA_DIR", "/data"),
		HTTPSAddr:  getenv("COPYSYNC_HTTPS_ADDR", ":8443"),
		ServerName: getenv("COPYSYNC_SERVER_NAME", hostname),
		AdminUser:  getenv("COPYSYNC_ADMIN_USER", "admin"),
		AdminPass:  getenv("COPYSYNC_ADMIN_PASS", "changeme"),
		APIKey:     getenv("COPYSYNC_API_KEY", ""),
		LogLevel:   getenv("COPYSYNC_LOG_LEVEL", "info"),
	}
	if hosts := os.Getenv("COPYSYNC_TLS_HOSTS"); hosts != "" {
		for _, h := range strings.Split(hosts, ",") {
			if h = strings.TrimSpace(h); h != "" {
				c.TLSHosts = append(c.TLSHosts, h)
			}
		}
	}
	return c
}
