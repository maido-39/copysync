# CopySync

Self-hosted, cross-platform **real-time clipboard sync** over your LAN through a
central server. Copy on one device, paste on another — text now, images/files
next. Built for Android, Windows, and Linux.

> **Status:** The **server**, a headless **`copyctl`** reference client, and the
> **Android** client (text sync) are implemented and verified — the Android app
> was tested end-to-end against the server on an Android 16 emulator. The desktop
> (Tauri) client and image/file sync are next — see [Roadmap](#roadmap).

## Why

- **Real-time sync** across all your devices on the local network.
- **Clipboard queuing** — items for an offline device are held and delivered when
  it reconnects.
- **All clipboard types** — text now; images/files (as references) next.
- **Offline history + search** — kept locally on each client (planned).
- **Push notifications** for clips copied on other devices (planned, client-side).
- **Routing** — broadcast to all devices by default, or pick specific targets.
- **Private** — self-signed TLS with certificate pinning; OTP pairing; the server
  can be made zero-knowledge with end-to-end encryption (Stage 3).

## Architecture

```
 Android ─┐                         ┌────────────────────────────┐
 (Kotlin) ├──── WSS control ───────►│  CopySync server (Go)       │
 Windows ─┤     HTTPS blob ────────►│  • WebSocket relay + queue  │
 (Tauri)  │                         │  • OTP pairing / registry   │
 Linux  ──┘                         │  • Admin SPA (ID/PW)        │
                                    │  • self-signed TLS + Docker │
                                    └────────────────────────────┘
```

The server is a **relay with a bounded offline queue**. Long-term history and
search live on each client. See [`docs/PROTOCOL.md`](docs/PROTOCOL.md) for the
wire contract.

```
server/             Go relay server (this milestone)
server/cmd/copyctl  Reference CLI client + protocol conformance harness
clients/android/    Kotlin + Jetpack Compose client (text sync — implemented)
clients/desktop/    Tauri v2 + Svelte desktop client (planned)
docs/PROTOCOL.md    wire protocol — source of truth for all clients
```

## Quick start (Docker)

```bash
docker compose up --build -d
```

Then open the admin UI at **https://<server-ip>:8443** (accept the self-signed
certificate warning). First login uses `admin` / `changeme` and **forces** you to
set a new password.

State (database, certificate, blobs) persists in the `copysync-data` volume. The
certificate is generated once and reused so the pin stays stable — do not delete
the volume unless you intend to re-pair every device.

## Quick start (local / development)

Requires **Go 1.25+**.

```bash
cd server
go build -o copysyncd ./cmd/copysyncd
COPYSYNC_DATA_DIR=./data COPYSYNC_HTTPS_ADDR=:8443 ./copysyncd
```

Run the tests (unit + WebSocket relay integration):

```bash
cd server
go test ./...
go test -race ./internal/transport/...   # relay core under the race detector
```

## Reference CLI client (`copyctl`)

`copyctl` is a headless client that speaks the full protocol (OTP pairing + SPKI
pinning, the WebSocket channel, and the blob channel). It's a real client for
headless/SSH boxes, and the conformance harness the GUI clients are ported from.

```bash
cd server && go build -o copyctl ./cmd/copyctl

# Pair (omit --pin to trust-on-first-use; otherwise pass the server's SPKI pin):
./copyctl pair --server https://192.168.1.10:8443 --otp 12345678 --name laptop-a

./copyctl send  --text "hello"        # send one text clip
./copyctl send  --file ./photo.png    # send a file via the blob channel
./copyctl watch                       # print/save incoming clips (no clipboard needed)
./copyctl run                         # two-way OS clipboard sync (Wayland/X11)
./copyctl history --search token      # search the local clipboard log
```

On a desktop with `wl-clipboard` or `xclip`, `run` syncs the real OS clipboard;
on a headless host it runs receive-only.

## Android client

`clients/android` is a native Kotlin + Jetpack Compose app (target API 36, min
API 29). Build a debug APK with the Gradle wrapper (needs JDK 17 + the Android
SDK; set `ANDROID_HOME` or `local.properties`):

```bash
cd clients/android && ./gradlew :app:assembleDebug
```

Because Android forbids background clipboard reads, the app uses the
production-proven workaround (see [`docs/PROTOCOL.md`](docs/PROTOCOL.md) and the
in-app setup screen): grant `READ_LOGS` and the overlay permission once via ADB —

```bash
adb shell pm grant com.copysync.android android.permission.READ_LOGS
adb shell appops set com.copysync.android SYSTEM_ALERT_WINDOW allow
```

— then pair from the app (server URL, OTP, device name) and it syncs in the
background via a foreground service. Verified end-to-end on an Android 16 emulator:
ADB grant works, pairing over pinned TLS succeeds, and text syncs both ways with
on-device history. Images/files are a later stage.

## Pairing a device

1. Log into the admin UI → **Pair a device** → **Generate pairing code**.
2. A one-time code + QR appear (the QR encodes server id/name, host, port,
   **SPKI pin**, and the code).
3. On the client, scan the QR or enter the values manually. The client pins the
   certificate and redeems the code; the server issues a long-lived device token.

OTPs are single-use and expire quickly. Revoke a device anytime from the admin
device list.

## Configuration

Boot-time environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `COPYSYNC_DATA_DIR` | `/data` | DB, certificate, and blobs live here |
| `COPYSYNC_HTTPS_ADDR` | `:8443` | TLS listen address |
| `COPYSYNC_SERVER_NAME` | hostname | human-readable server name |
| `COPYSYNC_TLS_HOSTS` | auto | extra SAN entries (comma-separated IPs/hosts) |
| `COPYSYNC_ADMIN_USER` | `admin` | seed admin username |
| `COPYSYNC_ADMIN_PASS` | `changeme` | seed password (must be changed on first login) |
| `COPYSYNC_LOG_LEVEL` | `info` | `debug`/`info`/`warn`/`error` |

Runtime settings editable from the admin UI: max message size, blob size/store
caps, offline queue depth & TTL, blob TTL, session/OTP TTLs, and the E2E toggle.

## Security model

- Self-signed TLS pinned by **SPKI SHA-256** (no public CA needed on a LAN).
- Device auth via bearer tokens (only the HMAC is stored server-side).
- Admin: bcrypt password, forced first-run change, HttpOnly/Secure/SameSite
  cookies, CSRF header on mutations, rate-limited login & pairing.
- End-to-end encryption (zero-knowledge server) and SPAKE2 pairing are planned
  for Stage 3.

## Roadmap

| Stage | Server | Desktop | Android |
|---|---|---|---|
| **S1 (now)** | TLS+pin, admin, OTP pairing, registry, **text relay**, presence, offline queue, Docker | text sync, history, pairing | background capture, text sync, FGS |
| **S2** | **blob channel** (images/files), GC/retention | images/files, routing UI | images/files, QR, Shizuku |
| **S3** | E2E, SPAKE2 pairing | E2E, keyring | E2E, Keystore |

## License

TBD.
