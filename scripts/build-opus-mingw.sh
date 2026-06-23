#!/usr/bin/env bash
# Cross-build a static libopus for the Windows GNU target (x86_64-w64-mingw32),
# so the Windows `.exe` can be built with the `audio-opus` feature.
#
# Why: audiopus_sys 0.1.8's vendored build doesn't cross-compile (its build.rs
# reads the *host* cfg, so it builds a Linux .so and never passes `--host`). We
# build libopus ourselves with the mingw toolchain and point audiopus_sys at the
# result via OPUS_LIB_DIR (see scripts/cargo-win-opus.sh / docs).
#
# Requires: x86_64-w64-mingw32-gcc, autoreconf/automake/libtoolize, make.
# Output:   third_party/opus-mingw/lib/libopus.a  (+ headers under include/)
set -euo pipefail

HOST=x86_64-w64-mingw32
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFIX="$ROOT/third_party/opus-mingw"
WORK="$ROOT/third_party/opus-src"

# Reuse the opus source vendored inside the audiopus_sys crate.
SRC="$(find "$HOME/.cargo/registry/src" -maxdepth 2 -type d -name 'audiopus_sys-*' | head -1)/opus"
[ -d "$SRC" ] || { echo "opus source not found under audiopus_sys; run a cargo fetch first" >&2; exit 1; }

rm -rf "$WORK" "$PREFIX"
mkdir -p "$ROOT/third_party"
cp -r "$SRC" "$WORK"
cd "$WORK"

./autogen.sh
./configure --host="$HOST" \
    --prefix="$PREFIX" \
    --enable-static --disable-shared \
    --disable-doc --disable-extra-programs \
    CC="${HOST}-gcc" AR="${HOST}-ar" RANLIB="${HOST}-ranlib"
make -j"$(nproc)"
make install

echo
echo "built: $PREFIX/lib/libopus.a"
echo "use with: OPUS_STATIC=1 OPUS_NO_PKG=1 OPUS_LIB_DIR=$PREFIX/lib \\"
echo "  cargo build -p deskoryn-daemon --features windows,audio-opus --target $HOST"
