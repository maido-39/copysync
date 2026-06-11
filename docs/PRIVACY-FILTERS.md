# Privacy filters + UI unification — spec

Status: classifier implemented & unit-tested in `copysync-core` (Rust); wiring rolls
out per client. Mirror the classifier byte-for-byte in Kotlin (Android) and Go
(copyctl), exactly like the E2E primitive.

## 1. Sensitivity classifier (shared)

`classify(text, customRegexes) -> Sensitivity?` — pure, client-side. Order matters
(structured wins over the heuristic so the UX wording is precise):

| Class | Rule |
|---|---|
| `private_key` | contains `-----BEGIN` … `PRIVATE KEY` |
| `otp_auth` | starts with `otpauth://` (case-insensitive) |
| `credit_card` | only digits/space/dash, 13–19 digits, **Luhn** valid |
| `custom` | matches any user regex |
| `password_like` | single token (no whitespace), 8–64 chars, **≥3 of 4** char classes (lower/upper/digit/symbol), Shannon ≥ 2.5 bits/char, **not** a URL/email/path |

Design choices (see research): Android `EXTRA_IS_SENSITIVE` is a *hint* the source
app sets on passwords/cards — Android additionally treats a clip with that flag as
sensitive regardless of the heuristic. The heuristic is deliberately conservative
(≥3 classes, entropy floor, URL/email/path guards) to avoid refusing to sync normal
copies. Tunable later via settings.

## 2. Behaviors

Two policies consume the classifier:

### (a) Sync exclusion — "don't share sensitive clips" (sender-side)
At **capture**, if `classify` matches and exclusion is enabled, the clip is **not
sent** to other devices. It may still be recorded locally (so the user sees what they
copied) but is flagged sensitive → purged quickly (b). Images/files are never
classified as passwords; this applies to text.

### (b) History quick-purge — "passwords don't linger" (both sides)
Any clip flagged sensitive (captured OR received) is recorded with a `sensitive` mark
and **auto-deleted from local history after a short TTL** (default 45 s). Implemented
as: persist the flag + a periodic sweep (`DELETE … WHERE sensitive=1 AND ts < now-TTL`)
on startup and on an interval, so it survives restarts.

### Settings (per client)
- `excludeSensitive` (bool, default **on**)
- `sensitiveHistorySecs` (int, default **45**, 0 = keep)
- `customPatterns` (list of regex strings; native regex per platform)
- Android also reads `EXTRA_IS_SENSITIVE`.

## 3. Roll-out

| Client | classifier | exclude (a) | purge (b) |
|---|---|---|---|
| desktop (Rust) | ✅ `privacy.rs` + tests | wiring | wiring |
| Android (Kotlin) | port | port | port (+ EXTRA_IS_SENSITIVE) |
| copyctl (Go) | port | port | port |

The server is **zero-knowledge under E2E** and must not do this — detection is
client-side only.

---

## Appendix — UI structure unification (separate work item)

Goal: desktop + Android (+ admin) share one mental model.

Proposed unified client tabs (desktop window + Android bottom nav):

1. **연결 / Home** — connection status, current pool (switcher), quick "what's syncing".
2. **기록 / History** — searchable list with thumbnails; sensitive items badged.
3. **설정 / Settings** — pairing/server, routing (targets), pools, size limits, E2E,
   **privacy filters** (this doc), at-rest encryption, autostart.
4. **디버깅 / Debug** — event log toggle + copy/share (already on Android).

Intuitiveness: consistent icons + ordering across platforms; prominent primary action
per tab; clear empty states; group settings by section with one-line help (admin
already does this). Admin web keeps its own nav but aligns naming (개요/기기/페어링/
설정/모니터링/계정).
