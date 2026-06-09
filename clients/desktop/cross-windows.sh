#!/usr/bin/env bash
# Cross-compile the desktop client from Linux to x86_64-pc-windows-gnu, to verify
# Windows portability without a Windows machine. (The *supported* distribution
# path is `cargo tauri build` on Windows / the CI workflow — see README. Tauri's
# gnu cross-build links here but is not officially supported for release.)
#
# Toolchain (conda-forge, no root needed):
#   mamba install -n csdesk -c conda-forge \
#     rust-std-x86_64-pc-windows-gnu=$(rustc --version | awk '{print $2}') gcc_win-64
set -e
source ~/miniforge3/etc/profile.d/conda.sh
conda activate csdesk

export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar

TARGET=x86_64-pc-windows-gnu
cd "$(dirname "$0")"

echo "=== core (lib) ==="
cargo build -p copysync-core --target "$TARGET" "$@"
echo "=== interop.exe (headless harness) ==="
cargo build -p copysync-core --example interop --target "$TARGET" "$@"
echo "=== full Tauri GUI app ==="
cargo build -p copysync-desktop --target "$TARGET" "$@"

echo
echo "Artifacts:"
file target/$TARGET/debug/examples/interop.exe
file target/$TARGET/debug/copysync-desktop.exe
