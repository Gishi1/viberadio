#!/usr/bin/env bash
#
# Package a Linux AppImage: the binary and its shared libraries in one file.
#
#   scripts/package-appimage.sh            # write to dist/
#   scripts/package-appimage.sh /tmp/rel   # write somewhere else
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT="${1:-$ROOT/dist}"
VERSION="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"
ARCH="$(uname -m)"
NAME="viberadio-${VERSION}-linux-${ARCH}"
APPDIR="$OUT/AppDir"
TOOLS="${TMPDIR:-/tmp}/viberadio-appimage-tools"

if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
  CARGO_BUILD_JOBS=2
  export CARGO_BUILD_JOBS
fi

echo "==> cargo build --release --locked (jobs: ${CARGO_BUILD_JOBS})"
cargo build --release --locked

mkdir -p "$TOOLS"
fetch() {
  if [ ! -x "$2" ]; then
    echo "==> downloading $(basename "$2")"
    curl -fsSL --retry 3 -o "$2" "$1"
    chmod +x "$2"
  fi
}

fetch \
  "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage" \
  "$TOOLS/linuxdeploy"
fetch \
  "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage" \
  "$TOOLS/appimagetool"

mkdir -p "$OUT"
if [ -d "$APPDIR" ]; then
  rm -rf "$APPDIR"
fi

echo "==> linuxdeploy"
"$TOOLS/linuxdeploy" --appimage-extract-and-run \
  --appdir "$APPDIR" \
  --executable target/release/viberadio \
  --desktop-file packaging/viberadio.desktop \
  --icon-file packaging/viberadio.svg

echo "==> appimagetool"
"$TOOLS/appimagetool" --appimage-extract-and-run \
  "$APPDIR" "$OUT/${NAME}.AppImage"

ls -lh "$OUT/${NAME}.AppImage"
