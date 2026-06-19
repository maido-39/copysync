#!/usr/bin/env bash
# Cross-compile the NATIVE CopySync client (copysync-agent + copysync-gui) from
# Linux to Windows (x86_64-pc-windows-gnu). Testing-grade — the *distribution*
# build is `cargo build` on Windows (MSVC) or the CI workflow. Needs the mingw
# toolchain (x86_64-w64-mingw32-*) and the windows-gnu rust-std on PATH.
#
# Produces: target/x86_64-pc-windows-gnu/release/copysync-{agent,gui}.exe
set -euo pipefail
cd "$(dirname "$0")"

export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++

# tray-icon's libxdo dep is Linux-only; the Windows target does not pull it.
cargo build --release --target x86_64-pc-windows-gnu -p copysync-agent -p copysync-gui

out=target/x86_64-pc-windows-gnu/release
echo "built:"
echo "  $out/copysync-agent.exe   (headless daemon — autostart target)"
echo "  $out/copysync-gui.exe     (window; auto-spawns the agent)"
