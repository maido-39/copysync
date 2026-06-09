# CopySync desktop client (Tauri + Rust)

A cross-platform (Windows / Linux) GUI client. All protocol, crypto and
networking live in the **`core`** library crate (`copysync-core`); the
**`src-tauri`** crate is a thin Tauri v2 shell with a no-framework HTML/JS UI.

```
core/        copysync-core — protocol, SPKI-pinned TLS, WS, blob, E2E, history,
             clipboard.  UI-agnostic and headlessly testable.
core/examples/interop.rs    CLI harness used for cross-impl interop verification.
src-tauri/   Tauri v2 app (commands + reconnecting sync actor + clipboard poller).
ui/          Static SPA (상태 / 기록 / 페어링 / 설정 tabs).
```

The core reuses the exact wire format of the Go server and `copyctl`:
OTP pairing, SPKI-SHA256 certificate pinning (custom `rustls` verifier),
the WebSocket control channel, the content-addressed blob channel, and
**end-to-end encryption** (Argon2id + AES-256-GCM, byte-compatible with the Go
and Android clients — verified).

## Toolchain

Tauri v2 needs Rust + the system webview. On this project we provision it with
conda-forge (no root needed):

```bash
mamba create -n csdesk -c conda-forge \
  rust pkg-config c-compiler make cmake \
  webkit2gtk4.1 gtk3 libsoup librsvg gdk-pixbuf cairo pango atk zlib \
  expat libpng freetype brotli pcre2 libffi graphite2 icu harfbuzz libxml2
mamba activate csdesk
```

(On a normal Linux box: `rustup` + `webkit2gtk-4.1`, `libsoup-3.0`, `gtk+-3.0`
dev packages. On Windows: `rustup` + WebView2, no GTK needed.)

## Build & run

```bash
cd clients/desktop
cargo build -p copysync-desktop     # builds the GUI (links webkit2gtk on Linux)
cargo run   -p copysync-desktop     # launch (needs a desktop session / DISPLAY)
```

Pair from the **페어링** tab (server URL + OTP from the admin UI; optional SPKI
pin and E2E passphrase). Once paired it auto-connects on launch and:

- mirrors the OS clipboard both ways — **text and images** (images are PNG-encoded
  to the blob channel and applied back to the clipboard on receipt);
- **sends files** via the 파일 보내기 button (eager upload, or advertised
  on-demand and served when a peer requests it, per the server threshold);
- shows an **OS notification** on every incoming clip;
- supports **per-device routing** — broadcast to all (default) or tick specific
  devices in the 전송 대상 card (roster updates live via presence);
- keeps a searchable local history and reconnects on drop;
- lives in the **system tray** (left-click opens; menu has 열기 / 종료) — closing
  the window hides to tray, sync keeps running — and can **launch on boot** (toggle
  in 설정). On Linux the tray needs `libayatana-appindicator3` installed at runtime.
- syncs **rich text (HTML)** too (`arboard` get/set HTML), with a plain-text fallback.

## Windows

It's the same crate — on Windows, Tauri targets **WebView2** (preinstalled on
Windows 10/11) and `arboard` uses the Windows clipboard; no GTK/webkit needed.

**Build natively on Windows** (the supported distribution path):

```powershell
rustup default stable                  # MSVC toolchain
cargo install tauri-cli --version "^2.0" --locked
cd clients\desktop
cargo tauri build                      # → target\release\bundle\{msi,nsis}\*
```

That produces an `.msi` and an NSIS `.exe` installer. CI does this automatically
on a `v*` tag — see [`.github/workflows/desktop.yml`](../../.github/workflows/desktop.yml)
(uploads `copysync-windows` artifacts).

**Cross-compile from Linux** (portability check, not for release): the whole
client — core *and* the GUI app — cross-compiles to `x86_64-pc-windows-gnu` and
links a real `.exe`, verified with `./cross-windows.sh` (needs
`rust-std-x86_64-pc-windows-gnu` + `gcc_win-64` from conda-forge). Tauri's gnu
cross-build is not officially supported for release, so ship via the MSVC path
above; the cross-build just proves the Windows port is sound.

> Gotcha: installing `gcc_win-64` makes the conda env export `CC=x86_64-w64-mingw32-cc`
> as the default, which breaks **native Linux** builds (host C deps try to use the
> Windows compiler). For native builds in that env, pin the host toolchain:
> `export CC=x86_64-conda-linux-gnu-cc CXX=x86_64-conda-linux-gnu-c++ AR=x86_64-conda-linux-gnu-ar`.
> The cross-build uses target-suffixed `*_x86_64_pc_windows_gnu` vars and is unaffected.

## Tests

The hard parts are verified headlessly — no display required:

```bash
cargo test -p copysync-core         # unit tests (protocol, pinning, E2E, history)
bash verify_interop.sh              # live Rust <-> Go server <-> copyctl interop:
                                    #   E2E text both ways, E2E file byte-identity,
                                    #   zero-knowledge server, wrong-passphrase reject
```

`verify_interop.sh` boots a real `copysyncd`, mints OTPs via the admin API, pairs
both a Rust (`interop`) and a Go (`copyctl`) device with the same passphrase, and
asserts cross-implementation interop end to end.

> The GUI window itself requires a real desktop session; it cannot be smoke-tested
> on a headless CI box (GTK fails to initialize without a display). The networking
> /crypto/history logic that the GUI drives is fully covered by the tests above.
