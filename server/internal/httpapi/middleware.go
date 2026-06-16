package httpapi

import (
	"crypto/subtle"
	"encoding/json"
	"net/http"
	"strings"
)

func (s *Server) recoverer(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			if rec := recover(); rec != nil {
				s.log.Error("panic in handler", "err", rec, "path", r.URL.Path)
				writeJSONError(w, http.StatusInternalServerError, "internal", "internal server error")
			}
		}()
		next.ServeHTTP(w, r)
	})
}

func securityHeaders(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		h := w.Header()
		h.Set("X-Content-Type-Options", "nosniff")
		h.Set("X-Frame-Options", "DENY")
		h.Set("Referrer-Policy", "no-referrer")
		next.ServeHTTP(w, r)
	})
}

// apiKeyOK reports whether the request bears the configured admin API key, sent
// as `Authorization: Bearer <key>` or `X-API-Key`. Always false when no key is
// configured (feature disabled). Constant-time compare to avoid timing leaks.
func (s *Server) apiKeyOK(r *http.Request) bool {
	if s.apiKey == "" {
		return false
	}
	got := r.Header.Get("X-API-Key")
	if got == "" {
		if h := r.Header.Get("Authorization"); strings.HasPrefix(h, "Bearer ") {
			got = strings.TrimSpace(strings.TrimPrefix(h, "Bearer "))
		}
	}
	if got == "" {
		return false
	}
	return subtle.ConstantTimeCompare([]byte(got), []byte(s.apiKey)) == 1
}

// withSession requires a valid admin session and, on mutating requests, the CSRF
// header. When enforcePWGate is true it also blocks until the first-run password
// change is complete. A valid admin API key bypasses all three (non-cookie
// credential, immune to CSRF, deliberately provisioned).
func (s *Server) withSession(next http.HandlerFunc, enforcePWGate bool) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if s.apiKeyOK(r) {
			s.log.Info("admin API authorized via API key", "method", r.Method, "path", r.URL.Path, "remote", clientIP(r))
			next(w, r)
			return
		}
		if _, ok := s.currentSession(r); !ok {
			writeJSONError(w, http.StatusUnauthorized, "unauthorized", "login required")
			return
		}
		if isMutating(r.Method) && r.Header.Get(csrfHeader) == "" {
			writeJSONError(w, http.StatusForbidden, "csrf", "missing "+csrfHeader+" header")
			return
		}
		if enforcePWGate {
			if admin, found, _ := s.store.GetAdmin(); found && admin.MustChangePW {
				writeJSONError(w, http.StatusConflict, "must_change_password", "change the default password first")
				return
			}
		}
		next(w, r)
	}
}

func (s *Server) requireAdmin(next http.HandlerFunc) http.HandlerFunc {
	return s.withSession(next, true)
}
func (s *Server) requireSession(next http.HandlerFunc) http.HandlerFunc {
	return s.withSession(next, false)
}

func isMutating(method string) bool {
	switch method {
	case http.MethodPost, http.MethodPut, http.MethodDelete, http.MethodPatch:
		return true
	default:
		return false
	}
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeJSONError(w http.ResponseWriter, status int, code, msg string) {
	writeJSON(w, status, map[string]string{"error": code, "message": msg})
}
