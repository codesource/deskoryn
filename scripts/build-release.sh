#!/usr/bin/env bash
# Build release binaries for Linux and Windows.
#
# Linux  -> native (x86_64-unknown-linux-gnu), `linux` feature.
# Windows -> cross-compiled (x86_64-pc-windows-gnu) via mingw-w64, `windows` feature.
#
# Prereqs for the Windows cross build:
#   rustup target add x86_64-pc-windows-gnu
#   sudo apt install gcc-mingw-w64-x86-64      # Debian/Ubuntu
#
# Output: dist/deskorynd (Linux) and dist/deskorynd.exe (Windows).
set -euo pipefail
cd "$(dirname "$0")/.."

mkdir -p dist

echo "==> Linux (native)"
cargo build --release -p deskoryn-daemon --features linux
cp target/release/deskorynd dist/deskorynd
strip dist/deskorynd || true

echo "==> Windows (cross, mingw)"
cargo build --release -p deskoryn-daemon --features windows --target x86_64-pc-windows-gnu
cp target/x86_64-pc-windows-gnu/release/deskorynd.exe dist/deskorynd.exe

echo
echo "Built:"
ls -la dist/
file dist/deskorynd dist/deskorynd.exe 2>/dev/null || true
