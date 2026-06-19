# CopySync native desktop client (agent + GUI)

A native, **non-WebView** desktop client, built to replace the Tauri/WebView app
for long-running stability. The Tauri app remains a separate, independent program;
this one shares only the `copysync-core` library.

## Why

A clipboard client lives hidden-in-the-tray for days. The Tauri/WebView shell is
a poor fit for that (e.g. Tauri #14088 — the window can crash ~50 min after being
hidden) and the UI could break. The native client removes the browser engine
entirely.

## Architecture

Two independent binaries that share the `copysync-core` engine library:

```
copysync-agent  — headless background daemon. Does ALL sync (clipboard capture +
                  apply, WebSocket control channel, blobs, E2E, SQLite history).
                  Runs for days with no UI. Autostarts at login. Exposes a
                  per-user local socket (named pipe on Windows / abstract socket
                  on Linux) speaking the copysync-ipc protocol.

copysync-gui    — eframe/egui window. A pure IPC client to the agent — no engine
                  code, no WebView. Auto-spawns the agent if it isn't running.
                  Tray icon, global hotkey, dark/light theme.
```

The engine lives in `copysync-core::engine` (`run` + `clipboard_loop`, behind an
`Emitter` trait + `EngineState`). Both the agent and the Tauri app drive it.

**Why a daemon + separate GUI:** the always-on critical part (sync) is decoupled
from the disposable UI. If the GUI crashes or is closed, sync keeps running.
On Windows the agent must be a **per-user session process** (NOT a Service) —
clipboard access requires the interactive desktop session.

## Build

Native (Linux/macOS; on this repo's conda toolchain pin the native CC):
```sh
cd clients/desktop
export CC=x86_64-conda-linux-gnu-cc CXX=x86_64-conda-linux-gnu-c++ AR=x86_64-conda-linux-gnu-ar
cargo build --release -p copysync-agent -p copysync-gui
```
(Linux GUI builds also need `libxdo3`/`libxdo-dev`, pulled in transitively by
`tray-icon`. Windows uses Win32 and does not.)

Windows (cross from Linux, gnu target — testing-grade; the distribution-grade
build is `cargo build` on Windows/MSVC or CI):
```sh
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
cargo build --release --target x86_64-pc-windows-gnu -p copysync-agent -p copysync-gui
```

## Install / run

- Put `copysync-agent[.exe]` and `copysync-gui[.exe]` in the same folder.
- Launch `copysync-gui` — it auto-spawns the agent. Pair from 설정 → 기기 페어링.
- Enable 설정 → "부팅 시 자동 시작": registers the **agent** for login start, so
  sync runs at boot even before you open the window.
- Closing the window hides to tray (sync continues). Tray → 종료 to quit the GUI.
- Global hotkey (default Ctrl+Shift+V) brings the window forward; configurable.

## Headless CLI (no GUI needed)

The agent doubles as a control CLI for scripting / servers:
```sh
copysync-agent serve                              # run the daemon
copysync-agent pair --server URL --otp CODE --name N [--pin B64] [--e2e PASS]
copysync-agent send "some text"
copysync-agent status | history | watch | discover | reconnect
```
