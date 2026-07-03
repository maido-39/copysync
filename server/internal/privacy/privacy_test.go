package privacy

// Mirrors clients/desktop/core/src/privacy.rs tests — keep the three
// implementations (Rust/Kotlin/Go) behaviorally identical.

import (
	"regexp"
	"testing"
)

func c(t string) Sensitivity { return Classify(t, nil) }

func TestStructured(t *testing.T) {
	if got := c("-----BEGIN OPENSSH PRIVATE KEY-----\nMIIB\n-----END OPENSSH PRIVATE KEY-----"); got != PrivateKey {
		t.Fatalf("private key: got %v", got)
	}
	if got := c("otpauth://totp/Acme:alice?secret=JBSWY3DPEHPK3PXP"); got != OtpAuth {
		t.Fatalf("otpauth: got %v", got)
	}
	if got := c("4242 4242 4242 4242"); got != CreditCard {
		t.Fatalf("card spaces: got %v", got)
	}
	if got := c("4242-4242-4242-4242"); got != CreditCard {
		t.Fatalf("card dashes: got %v", got)
	}
	if got := c("4242 4242 4242 4241"); got != 0 {
		t.Fatalf("bad luhn should pass: got %v", got)
	}
}

func TestPasswords(t *testing.T) {
	for _, s := range []string{"xK9$mQ2vL7!", "P@ssw0rd!2026", "Tr0ub4dour&3xtra"} {
		if got := c(s); got != PasswordLike {
			t.Fatalf("%q: got %v, want PasswordLike", s, got)
		}
	}
}

func TestNormalTextNotFlagged(t *testing.T) {
	for _, s := range []string{
		"hello world this is a normal sentence",
		"https://example.com/page?x=1",
		"alice@example.com",
		"meeting at 8:30 tomorrow",
		"CopySync",
		"/home/user/notes.txt",
		"1234",
		"the quick brown fox",
		"안녕하세요반갑습니다오늘회의",
		"ANDROID→랩탑-192204",
		"회의자료-2024-최종본-v3",
		"Report-2024-V2-final",
		"ABC-123-XYZ-789",
		"order_id_12345_abcDEF",
		"commit-a1b2c3d4e5f6",
	} {
		if got := c(s); got != 0 {
			t.Fatalf("%q wrongly flagged as %v", s, got)
		}
	}
}

func TestCustomPattern(t *testing.T) {
	res := []*regexp.Regexp{regexp.MustCompile(`(?i)\bsk-[a-z0-9]{20,}\b`)}
	if got := Classify("sk-abcdefghijklmnopqrstuvwxyz1234", res); got != Custom {
		t.Fatalf("custom: got %v", got)
	}
	if got := Classify("just a normal note", res); got != 0 {
		t.Fatalf("normal with custom: got %v", got)
	}
}
