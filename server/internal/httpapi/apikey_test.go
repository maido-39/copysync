package httpapi

import (
	"net/http"
	"testing"
)

func apiKeyReq(hdr map[string]string) *http.Request {
	r, _ := http.NewRequest(http.MethodGet, "/admin/devices", nil)
	for k, v := range hdr {
		r.Header.Set(k, v)
	}
	return r
}

func TestAPIKeyOK(t *testing.T) {
	const key = "abcdefghijklmnopqrstuvwxyz012345" // >= 24 chars

	cases := []struct {
		name   string
		apiKey string
		hdr    map[string]string
		want   bool
	}{
		{"disabled when no key configured", "", map[string]string{"X-API-Key": key}, false},
		{"x-api-key correct", key, map[string]string{"X-API-Key": key}, true},
		{"bearer correct", key, map[string]string{"Authorization": "Bearer " + key}, true},
		{"bearer tolerates surrounding spaces", key, map[string]string{"Authorization": "Bearer  " + key + " "}, true},
		{"wrong key rejected", key, map[string]string{"X-API-Key": "not-the-key-not-the-key-xx"}, false},
		{"no credential rejected", key, map[string]string{}, false},
		{"empty bearer rejected", key, map[string]string{"Authorization": "Bearer "}, false},
		{"non-bearer scheme ignored", key, map[string]string{"Authorization": "Basic " + key}, false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			s := &Server{apiKey: c.apiKey}
			if got := s.apiKeyOK(apiKeyReq(c.hdr)); got != c.want {
				t.Fatalf("apiKeyOK = %v, want %v", got, c.want)
			}
		})
	}
}
