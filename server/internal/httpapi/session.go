package httpapi

import (
	"net/http"
	"time"

	"github.com/syaro/copysync/internal/auth"
	"github.com/syaro/copysync/internal/model"
)

const (
	sessionCookie = "copysync_session"
	csrfHeader    = "X-CopySync-CSRF"
)

// issueSession creates a session for username and sets the session cookie.
func (s *Server) issueSession(w http.ResponseWriter, username string) error {
	settings, _ := s.store.GetSettings()
	raw, idHash := auth.NewSessionID()
	now := s.now()
	sess := model.Session{
		IDHash:    idHash,
		Username:  username,
		CreatedAt: now,
		ExpiresAt: now.Add(time.Duration(settings.SessionTTLSeconds) * time.Second),
	}
	if err := s.store.PutSession(sess); err != nil {
		return err
	}
	http.SetCookie(w, &http.Cookie{
		Name:     sessionCookie,
		Value:    raw,
		Path:     "/",
		HttpOnly: true,
		Secure:   true,
		SameSite: http.SameSiteStrictMode,
		Expires:  sess.ExpiresAt,
	})
	return nil
}

// currentSession returns the valid session for the request, if any.
func (s *Server) currentSession(r *http.Request) (model.Session, bool) {
	cookie, err := r.Cookie(sessionCookie)
	if err != nil {
		return model.Session{}, false
	}
	sess, found, err := s.store.GetSession(auth.HashSessionID(cookie.Value))
	if err != nil || !found {
		return model.Session{}, false
	}
	if s.now().After(sess.ExpiresAt) {
		_ = s.store.DeleteSession(sess.IDHash)
		return model.Session{}, false
	}
	return sess, true
}

// clearSession deletes the request's session and expires the cookie.
func (s *Server) clearSession(w http.ResponseWriter, r *http.Request) {
	if cookie, err := r.Cookie(sessionCookie); err == nil {
		_ = s.store.DeleteSession(auth.HashSessionID(cookie.Value))
	}
	http.SetCookie(w, &http.Cookie{
		Name:     sessionCookie,
		Value:    "",
		Path:     "/",
		HttpOnly: true,
		Secure:   true,
		SameSite: http.SameSiteStrictMode,
		MaxAge:   -1,
	})
}
