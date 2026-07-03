// Package privacy is the client-side sensitivity classifier for the privacy
// filter — the Go port of the desktop's clients/desktop/core/src/privacy.rs
// (mirrored byte-for-byte in behavior; keep the three implementations in sync).
//
// It detects clipboard content that should not leave the device (sync
// exclusion): passwords, OTP/2FA secrets, payment cards, private keys, and
// user-supplied patterns. Pure and dependency-light so it is unit-testable and
// reusable by copyctl (and the server, should it ever need a fallback check).
package privacy

import (
	"math"
	"regexp"
	"strings"
	"unicode"
)

// Sensitivity says why a clip was flagged (drives both policy and UX wording).
type Sensitivity int

const (
	PrivateKey Sensitivity = iota + 1
	OtpAuth
	CreditCard
	PasswordLike
	Custom
)

// Label matches the Rust Sensitivity::label wording exactly.
func (s Sensitivity) Label() string {
	switch s {
	case PrivateKey:
		return "private key"
	case OtpAuth:
		return "OTP secret"
	case CreditCard:
		return "payment card"
	case PasswordLike:
		return "password-like"
	case Custom:
		return "custom pattern"
	}
	return "sensitive"
}

// Classify inspects a clip and reports why it is sensitive, or 0 (not
// sensitive). custom holds user-supplied patterns (nil for none). Structured
// matches win over the password heuristic to keep wording precise.
func Classify(text string, custom []*regexp.Regexp) Sensitivity {
	t := strings.TrimSpace(text)
	if t == "" {
		return 0
	}
	if strings.Contains(t, "-----BEGIN") && strings.Contains(t, "PRIVATE KEY") {
		return PrivateKey
	}
	if len(t) >= 10 && strings.EqualFold(t[:10], "otpauth://") {
		return OtpAuth
	}
	if isCardNumber(t) {
		return CreditCard
	}
	for _, re := range custom {
		if re != nil && re.MatchString(t) {
			return Custom
		}
	}
	if isPasswordLike(t) {
		return PasswordLike
	}
	return 0
}

// isCardNumber: 13–19 digits (allowing spaces/dashes, nothing else) passing Luhn.
func isCardNumber(t string) bool {
	digits := make([]int, 0, len(t))
	for _, c := range t {
		switch {
		case c >= '0' && c <= '9':
			digits = append(digits, int(c-'0'))
		case c == ' ' || c == '-':
		default:
			return false
		}
	}
	return len(digits) >= 13 && len(digits) <= 19 && luhnOK(digits)
}

func luhnOK(digits []int) bool {
	sum := 0
	for i := 0; i < len(digits); i++ {
		v := digits[len(digits)-1-i]
		if i%2 == 1 {
			v *= 2
			if v > 9 {
				v -= 9
			}
		}
		sum += v
	}
	return sum%10 == 0
}

func looksLikeURL(t string) bool {
	l := strings.ToLower(t)
	return strings.Contains(l, "://") || strings.HasPrefix(l, "http") || strings.HasPrefix(l, "www.")
}

func looksLikeEmail(t string) bool {
	parts := strings.Split(t, "@")
	if len(parts) != 2 {
		return false
	}
	u, d := parts[0], parts[1]
	return u != "" && strings.Contains(d, ".") && !strings.HasPrefix(d, ".") && !strings.HasSuffix(d, ".")
}

func looksLikePath(t string) bool {
	if strings.HasPrefix(t, "/") || strings.HasPrefix(t, "./") || strings.HasPrefix(t, "../") {
		return true
	}
	// Windows drive path, e.g. C:\ or C:/
	return len(t) >= 3 && t[1] == ':' && (t[2] == '\\' || t[2] == '/')
}

// isPasswordLike: a single token (no whitespace), 8–64 chars, that looks like a
// *random credential*: a strong special symbol plus mixed letters/digits with
// random-ish entropy. Deliberately conservative — excludes URLs/emails/paths,
// CJK/natural-language scripts, and hyphen/dot-separated identifiers. Better to
// occasionally sync a weak password than to silently drop normal copies.
func isPasswordLike(t string) bool {
	runes := []rune(t)
	n := len(runes)
	if n < 8 || n > 64 {
		return false
	}
	for _, c := range runes {
		if unicode.IsSpace(c) {
			return false
		}
	}
	if looksLikeURL(t) || looksLikeEmail(t) || looksLikePath(t) {
		return false
	}
	for _, c := range runes {
		if isCJK(c) {
			return false
		}
	}
	var lo, up, di, sy bool
	for _, c := range runes {
		switch {
		case c >= 'a' && c <= 'z':
			lo = true
		case c >= 'A' && c <= 'Z':
			up = true
		case c >= '0' && c <= '9':
			di = true
		case isStrongSymbol(c):
			sy = true
		}
	}
	alnum := 0
	for _, b := range []bool{lo, up, di} {
		if b {
			alnum++
		}
	}
	return sy && alnum >= 2 && shannonBitsPerChar(runes) >= 2.5
}

// isStrongSymbol: a "password" special char — excludes separators common in
// filenames/IDs ('-' '_' '.' '/' ':' ',').
func isStrongSymbol(c rune) bool {
	if unicode.IsLetter(c) || unicode.IsDigit(c) {
		return false
	}
	switch c {
	case '-', '_', '.', '/', ':', ',':
		return false
	}
	return true
}

// isCJK: Hangul / Kana / CJK ideographs — natural language, not credentials.
func isCJK(c rune) bool {
	switch {
	case c >= 0x1100 && c <= 0x11FF,
		c >= 0x3130 && c <= 0x318F,
		c >= 0xAC00 && c <= 0xD7A3,
		c >= 0x3040 && c <= 0x30FF,
		c >= 0x3400 && c <= 0x4DBF,
		c >= 0x4E00 && c <= 0x9FFF:
		return true
	}
	return false
}

// shannonBitsPerChar: Shannon entropy in bits per character (randomness signal).
func shannonBitsPerChar(runes []rune) float64 {
	if len(runes) == 0 {
		return 0
	}
	counts := make(map[rune]int, len(runes))
	for _, c := range runes {
		counts[c]++
	}
	n := float64(len(runes))
	var h float64
	for _, c := range counts {
		p := float64(c) / n
		h -= p * math.Log2(p)
	}
	return h
}

// CompilePatterns compiles user-supplied custom patterns, silently skipping
// invalid ones (reported via the returned slice of errors, one per bad pattern).
func CompilePatterns(patterns []string) ([]*regexp.Regexp, []error) {
	var res []*regexp.Regexp
	var errs []error
	for _, p := range patterns {
		re, err := regexp.Compile(p)
		if err != nil {
			errs = append(errs, err)
			continue
		}
		res = append(res, re)
	}
	return res, errs
}
