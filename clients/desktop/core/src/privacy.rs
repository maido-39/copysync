//! Client-side sensitivity classifier for the privacy filter.
//!
//! Detects clipboard content that should not leave the device (sync exclusion)
//! and/or should be purged from local history quickly: passwords, OTP/2FA secrets,
//! payment cards, private keys, and user-supplied patterns. Pure and
//! dependency-light so it can be unit-tested here and mirrored byte-for-byte in
//! the Android (Kotlin) and copyctl (Go) clients.

use regex::Regex;

/// Why a clip was flagged sensitive (drives both policy and UX wording).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    PrivateKey,
    OtpAuth,
    CreditCard,
    PasswordLike,
    Custom,
}

impl Sensitivity {
    pub fn label(&self) -> &'static str {
        match self {
            Sensitivity::PrivateKey => "private key",
            Sensitivity::OtpAuth => "OTP secret",
            Sensitivity::CreditCard => "payment card",
            Sensitivity::PasswordLike => "password-like",
            Sensitivity::Custom => "custom pattern",
        }
    }
}

/// Classify a clip. `custom` holds user-supplied regexes (pass `&[]` for none).
/// Structured matches win over the password heuristic to keep wording precise.
pub fn classify(text: &str, custom: &[Regex]) -> Option<Sensitivity> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if t.contains("-----BEGIN") && t.contains("PRIVATE KEY") {
        return Some(Sensitivity::PrivateKey);
    }
    // NB: char-boundary-safe — `t[..10]` would panic on multi-byte (e.g. Korean)
    // text, which used to crash the sync actor on every such copy.
    if t.get(..10).is_some_and(|p| p.eq_ignore_ascii_case("otpauth://")) {
        return Some(Sensitivity::OtpAuth);
    }
    if is_card_number(t) {
        return Some(Sensitivity::CreditCard);
    }
    for re in custom {
        if re.is_match(t) {
            return Some(Sensitivity::Custom);
        }
    }
    if is_password_like(t) {
        return Some(Sensitivity::PasswordLike);
    }
    None
}

/// 13–19 digits (allowing spaces/dashes, nothing else) passing the Luhn checksum.
fn is_card_number(t: &str) -> bool {
    if !t.chars().all(|c| c.is_ascii_digit() || c == ' ' || c == '-') {
        return false;
    }
    let digits: Vec<u8> = t.bytes().filter(u8::is_ascii_digit).map(|b| b - b'0').collect();
    (13..=19).contains(&digits.len()) && luhn_ok(&digits)
}

fn luhn_ok(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    for (i, &d) in digits.iter().rev().enumerate() {
        let mut v = d as u32;
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum % 10 == 0
}

fn looks_like_url(t: &str) -> bool {
    let l = t.to_ascii_lowercase();
    l.contains("://") || l.starts_with("http") || l.starts_with("www.")
}

fn looks_like_email(t: &str) -> bool {
    let mut parts = t.split('@');
    matches!((parts.next(), parts.next(), parts.next()),
        (Some(u), Some(d), None)
            if !u.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.'))
}

fn looks_like_path(t: &str) -> bool {
    t.starts_with('/') || t.starts_with("./") || t.starts_with("../")
        || (t.len() >= 3 && t.as_bytes()[1] == b':' && matches!(t.as_bytes()[2], b'\\' | b'/'))
}

/// A single token (no whitespace), 8–64 chars that looks like a *random
/// credential*: a strong special symbol plus mixed letters/digits, with
/// random-ish entropy. Deliberately conservative to avoid false positives on
/// everyday text — it excludes URLs/emails/paths, CJK/natural-language scripts,
/// and hyphen/dot-separated identifiers (filenames, order IDs) that merely look
/// busy. Better to occasionally sync a weak password than to silently drop the
/// user's normal copies.
fn is_password_like(t: &str) -> bool {
    let n = t.chars().count();
    if n < 8 || n > 64 || t.chars().any(char::is_whitespace) {
        return false;
    }
    if looks_like_url(t) || looks_like_email(t) || looks_like_path(t) {
        return false;
    }
    // CJK / natural-language scripts are essentially never passwords.
    if t.chars().any(is_cjk) {
        return false;
    }
    let (mut lo, mut up, mut di, mut sy) = (false, false, false, false);
    for c in t.chars() {
        if c.is_ascii_lowercase() {
            lo = true;
        } else if c.is_ascii_uppercase() {
            up = true;
        } else if c.is_ascii_digit() {
            di = true;
        } else if is_strong_symbol(c) {
            sy = true;
        }
    }
    // Require a strong special char (not just a '-'/'.' separator) plus ≥2 of
    // lower/upper/digit — "has a special char, is mixed, and looks random".
    let alnum = lo as u8 + up as u8 + di as u8;
    sy && alnum >= 2 && shannon_bits_per_char(t) >= 2.5
}

/// A "password" special character — excludes separators common in filenames/IDs.
fn is_strong_symbol(c: char) -> bool {
    !c.is_alphanumeric() && !matches!(c, '-' | '_' | '.' | '/' | ':' | ',')
}

/// Hangul / Kana / CJK ideographs — natural-language text, not credentials.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7A3
            | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF)
}

/// Shannon entropy in bits per character (a randomness signal).
fn shannon_bits_per_char(s: &str) -> f64 {
    use std::collections::HashMap;
    let mut counts: HashMap<char, usize> = HashMap::new();
    let mut n = 0usize;
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    let n = n as f64;
    -counts.values().map(|&c| {
        let p = c as f64 / n;
        p * p.log2()
    }).sum::<f64>()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn c(t: &str) -> Option<Sensitivity> {
        classify(t, &[])
    }

    #[test]
    fn structured() {
        assert_eq!(c("-----BEGIN OPENSSH PRIVATE KEY-----\nMIIB\n-----END OPENSSH PRIVATE KEY-----"), Some(Sensitivity::PrivateKey));
        assert_eq!(c("otpauth://totp/Acme:alice?secret=JBSWY3DPEHPK3PXP"), Some(Sensitivity::OtpAuth));
        assert_eq!(c("4242 4242 4242 4242"), Some(Sensitivity::CreditCard)); // valid Luhn test card
        assert_eq!(c("4242-4242-4242-4242"), Some(Sensitivity::CreditCard));
        assert_eq!(c("4242 4242 4242 4241"), None); // bad checksum
    }

    #[test]
    fn passwords() {
        assert_eq!(c("xK9$mQ2vL7!"), Some(Sensitivity::PasswordLike));
        assert_eq!(c("P@ssw0rd!2026"), Some(Sensitivity::PasswordLike));
        assert_eq!(c("Tr0ub4dour&3xtra"), Some(Sensitivity::PasswordLike));
    }

    #[test]
    fn normal_text_not_flagged() {
        assert_eq!(c("hello world this is a normal sentence"), None);
        assert_eq!(c("https://example.com/page?x=1"), None);
        assert_eq!(c("alice@example.com"), None);
        assert_eq!(c("meeting at 8:30 tomorrow"), None);
        assert_eq!(c("CopySync"), None); // single short word, 1 class
        assert_eq!(c("/home/user/notes.txt"), None); // path
        assert_eq!(c("1234"), None); // too short
        assert_eq!(c("the quick brown fox"), None);
        // Regression: real-world mixed text that the old heuristic over-flagged.
        assert_eq!(c("안녕하세요반갑습니다오늘회의"), None); // Korean (CJK)
        assert_eq!(c("ANDROID→랩탑-192204"), None); // CJK present
        assert_eq!(c("회의자료-2024-최종본-v3"), None); // CJK + digits + hyphen
        assert_eq!(c("Report-2024-V2-final"), None); // hyphen-separated id, no strong symbol
        assert_eq!(c("ABC-123-XYZ-789"), None); // identifier, separators only
        assert_eq!(c("order_id_12345_abcDEF"), None); // underscores only
        assert_eq!(c("commit-a1b2c3d4e5f6"), None); // hash-ish, no strong symbol
    }

    #[test]
    fn custom_pattern() {
        let re = vec![Regex::new(r"(?i)\bsk-[a-z0-9]{20,}\b").unwrap()];
        assert_eq!(classify("sk-abcdefghijklmnopqrstuvwxyz1234", &re), Some(Sensitivity::Custom));
        assert_eq!(classify("just a normal note", &re), None);
    }
}
