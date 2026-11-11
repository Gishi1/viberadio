#!/usr/bin/env bash
#
# Package a macOS build: binary, README and LICENSE in a .tar.gz.
#
#   scripts/package-macos.sh            # write to dist/
#   scripts/package-macos.sh /tmp/rel   # write somewhere else
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT="${1:-$ROOT/dist}"
VERSION="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"

case "$(uname -m)" in
  arm64) ARCH=aarch64 ;;
  x86_64) ARCH=x86_64 ;;
  *) ARCH="$(uname -m)" ;;
esac

NAME="viberadio-${VERSION}-macos-${ARCH}"

if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
  CARGO_BUILD_JOBS=2
  export CARGO_BUILD_JOBS
fi

echo "==> cargo build --release --locked (jobs: ${CARGO_BUILD_JOBS})"
cargo build --release --locked

mkdir -p "$OUT/$NAME"
cp target/release/viberadio "$OUT/$NAME/"
cp README.md LICENSE "$OUT/$NAME/"

echo "==> packaging ${NAME}.tar.gz"
tar -C "$OUT" -czf "$OUT/${NAME}.tar.gz" "$NAME"
ls -lh "$OUT/${NAME}.tar.gz"
