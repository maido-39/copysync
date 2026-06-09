# CopySync Wire Protocol (v1)

This document is the source of truth for how clients talk to the CopySync server.
The Go definitions live in `server/internal/protocol` and `server/internal/model`.

## Transport

Two channels, both over the same self-signed **HTTPS** listener (default `:8443`):

1. **Control channel** — a WebSocket at `GET /ws`. Carries small JSON frames:
   clipboard metadata, presence, acks. Each frame is a JSON **envelope**.
2. **Blob channel** — plain HTTPS at `/blob/{id}` (`PUT`/`GET`/`HEAD`). Carries
   large payloads (images, files, rich text) so they never block the control
   channel. (Implemented in server Pass B.)

Clients **pin** the server's self-signed certificate by its **SPKI SHA-256**
fingerprint (base64), delivered during pairing. Every later TLS connection must
match the pin.

## Envelope

Every control-channel frame is:

```json
{ "t": "<type>", "d": { ... } }
```

`t` is the message type; `d` is the type-specific payload.

## Control messages

### `hello` (client → server) — first frame after connecting
| field | type | notes |
|---|---|---|
| `deviceId` | string | issued at pairing |
| `deviceName` | string | human-readable, unique |
| `token` | string | long-lived bearer token from pairing |
| `platform` | string | `windows`/`linux`/`android`/… (informational) |
| `proto` | int | protocol version (currently `1`) |

### `hello_ok` (server → client) — handshake accepted
| field | type | notes |
|---|---|---|
| `serverId` | string | stable server id |
| `serverName` | string | human-readable server name |
| `e2e` | bool | whether E2E mode is enabled |
| `you` | Device | the caller's own device record |
| `roster` | DeviceInfo[] | all paired devices + online flags |
| `maxMsg` | int | max control-frame size in bytes |
| `blobCap` | int | max blob size in bytes |
| `onDemandThreshold` | int | files ≤ this are uploaded eagerly; larger ones are advertised on demand |

After `hello_ok`, the server immediately replays any **queued** clips (see
*Offline queue*), then live traffic begins.

### `hello_err` (server → client) — handshake rejected, then close
`{ "code": "...", "message": "..." }` — codes: `bad_hello`, `unauthorized`.

### `clip` (both directions) — a clipboard item
| field | type | notes |
|---|---|---|
| `id` | string | client-generated unique id (idempotency) |
| `originDeviceId` | string | set/over-written by the server to the authenticated sender |
| `seq` | uint64 | per-origin monotonic counter |
| `ts` | RFC3339 | client time (advisory; server fills if zero) |
| `mime` | string[] | ordered MIME preferences, e.g. `["image/png","text/plain"]` |
| `inlineText` | string? | small text only, and only when E2E is off |
| `blobId` | string? | content address (`sha256:<hex>`) for large payloads |
| `name` | string? | filename hint for file payloads |
| `onDemand` | bool? | `true` ⇒ bytes not uploaded; the origin holds them and uploads on a `blob_request` |
| `size` | int | payload size in bytes |
| `sha256` | string | hash of the payload (plaintext if E2E off, ciphertext if on) |
| `targets` | `"all"` \| string[] | recipients: all paired devices, or explicit ids |
| `enc` | object? | present ⇒ payload is E2E ciphertext `{alg,keyId}`; nonce is prepended to the ciphertext and `sha256`/`blobId` are over the ciphertext |

The server relays a `clip` to its targets (excluding the origin) and **queues**
it for any offline targets.

### `ack` (server → client) — disposition of a sent clip
`{ "id": "...", "status": "relayed"|"queued"|"rejected", "queuedFor": [ids] }`

### `blob_request` (server → client) — upload an on-demand blob now
`{ "id": "sha256:<hex>" }` — sent to the **origin** of an on-demand clip when
another device requests its bytes. The origin responds by `PUT`ting the blob.

### `presence` (server → client) — roster delta
`{ "device": Device, "online": bool }`

### `roster` (server → client) — full roster
`{ "devices": DeviceInfo[] }`

### `error` (server → client) — non-fatal
`{ "code": "...", "message": "..." }`

### Types
- **Device**: `{ id, name, platform, createdAt, lastSeenAt, revoked }`
- **DeviceInfo**: Device + `{ online: bool }`

## Echo-loop suppression (client responsibility)

When a client writes a received clip to its OS clipboard, the OS fires a change
event that would otherwise be re-broadcast forever. Each client must:

1. Record the `sha256` of the last item it **wrote** to the OS clipboard.
2. Ignore an inbound `clip` whose `sha256` equals that value.
3. Not re-broadcast a local change equal to one it just received.

The server preserves `originDeviceId` + `seq` so duplicates can be dropped
idempotently by `(originDeviceId, seq, sha256)`.

## Offline queue ("clipboard queuing")

If a target device is offline, the server stores the `clip` in a **bounded
per-device FIFO queue** (default depth 200, TTL 72h). On the device's next
`hello`, the server replays the queued clips in order, then clears the queue.
Oldest items are trimmed when the depth cap is exceeded.

## Blob channel (server Pass B)

Large payloads are content-addressed by `sha256`:
- `PUT /blob/{id}` — upload; the server verifies the body hashes to `{id}` and
  enforces the `blobCap`. Auth: `Authorization: Bearer <deviceToken>`.
- `GET /blob/{id}` — download. `HEAD /blob/{id}` — existence/size check.

A `clip` with a `blobId` is queued for offline devices with its blob **pinned**
(refcounted) so garbage collection cannot remove a blob a device still needs.

### On-demand large files (reference + pull)

Files larger than `onDemandThreshold` are **not uploaded** when copied. The origin
sends a `clip` with `blobId`, `name`, `size` and `onDemand: true`, and keeps the
bytes locally. When another device wants them, its `GET /blob/{id}` misses on the
server, which then sends a `blob_request` to the origin and **long-polls** (up to
60s) until the origin `PUT`s the blob, then streams it to the requester and caches
it (later requesters are served directly). If the origin is offline, `GET` returns
`404`; if it doesn't deliver in time, `504`.

## End-to-end encryption (optional, client-side)

When devices share a passphrase, each derives a 32-byte group key with
**Argon2id(passphrase, salt = sha256("copysync-e2e|" + serverId))** — the server
never sees the passphrase, so it cannot derive the key. Before sending, a client
seals the payload with **XChaCha20-Poly1305** as `nonce ‖ ciphertext‖tag`:
- text → `inlineText` = base64(nonce‖ct), and `sha256` = sha256(ciphertext);
- files → the blob bytes ARE the sealed blob and `blobId = sha256(ciphertext)`.

`enc = { alg: "xchacha20poly1305", keyId }` marks the clip (`keyId` = truncated
sha256 of the key, for fast mismatch detection). Because the server already never
inspects payloads, it relays and stores **only ciphertext** — it is
zero-knowledge. Receivers with the matching key decrypt; others see only
ciphertext. Server-originated admin broadcast is disabled while E2E is on.

(Implemented and verified in `copyctl`: text, eager files, and on-demand large
files all round-trip; the on-disk blob store holds only ciphertext. The Android
client port is next.)

## Pairing

1. **Admin** generates a one-time code: `POST /admin/pairing` →
   `{ otp, expiresAt, payload, qr }` where `payload` is:
   `{ serverId, serverName, host, port, spkiPin, otp }` (also encoded as a QR).
2. The new device verifies the server via `GET /pair/serverinfo`
   (`{ serverId, serverName, spkiPin, proto }`) and pins `spkiPin`.
3. The device redeems the code: `POST /pair/claim`
   `{ otp, deviceName, platform, pubkey? }` over the pinned TLS connection →
   `{ deviceId, token, serverId, serverName, e2e }`.

OTPs are single-use and short-lived. The bearer `token` is returned once; the
server stores only its HMAC.

## Security model (MVP)

- **Transport**: self-signed TLS + SPKI-pin (trust-on-first-use anchored by the
  out-of-band OTP/QR).
- **Device auth**: bearer token (HMAC-stored) presented in `hello` and on the
  blob channel.
- **Admin**: session cookie (HttpOnly/Secure/SameSite=Strict) + bcrypt; forced
  password change on first login; CSRF header on mutating requests; rate-limited
  login and pairing.

## Reserved for Stage 3 (E2E)

When `e2e` is on, clients encrypt clip payloads with XChaCha20-Poly1305 under a
shared device-group key (delivered wrapped during pairing). The server then sees
only metadata (`enc`, `size`, `sha256` of ciphertext) and relays opaquely; it
cannot preview content or broadcast on the user's behalf. Pairing may be upgraded
to a SPAKE2 PAKE to remove trust-on-first-use.

## Versioning

`proto` is bumped on incompatible changes. Servers accept clients with the same
major `proto`; unknown frame types are ignored for forward compatibility.
