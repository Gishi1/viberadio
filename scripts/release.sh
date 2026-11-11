#!/usr/bin/env bash
#
# Build a VibeRadio release: test, compile, package and checksum.
#
#   scripts/release.sh                  # test, build, package
#   scripts/release.sh --skip-tests     # package an existing checkout
#   scripts/release.sh --tag            # also create the v<version> git tag
#   scripts/release.sh --tag --push     # create and push the tag
#   scripts/release.sh --out /tmp/rel   # write artifacts somewhere else
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SKIP_TESTS=0
CREATE_TAG=0
PUSH_TAG=0
OUT="$ROOT/dist"

while [ $# -gt 0 ]; do
  case "$1" in
    --skip-tests) SKIP_TESTS=1 ;;
    --tag) CREATE_TAG=1 ;;
    --push) CREATE_TAG=1; PUSH_TAG=1 ;;
    --out)
      [ $# -ge 2 ] || { echo "--out needs a directory" >&2; exit 2; }
      OUT="$2"
      shift
      ;;
    -h|--help)
      sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      exit 2
      ;;
  esac
  shift
done

VERSION="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"
ARCH="$(uname -m)"
NAME="viberadio-${VERSION}-linux-${ARCH}"
STAGE="${OUT}/${NAME}"

echo "==> VibeRadio ${VERSION} (${ARCH})"

# Release builds of egui/wgpu are memory-hungry. Cap the default parallelism;
# set CARGO_BUILD_JOBS explicitly to use more.
if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
  CARGO_BUILD_JOBS=2
  export CARGO_BUILD_JOBS
fi

if [ "$SKIP_TESTS" -eq 0 ]; then
  echo "==> cargo test"
  cargo test --locked
fi

echo "==> cargo build --release --locked (jobs: ${CARGO_BUILD_JOBS})"
cargo build --release --locked

rm -rf "$STAGE"
mkdir -p "$STAGE"

install -Dm755 target/release/viberadio "$STAGE/viberadio"
install -Dm644 README.md "$STAGE/README.md"
install -Dm644 LICENSE "$STAGE/LICENSE"
install -Dm644 packaging/viberadio.desktop "$STAGE/viberadio.desktop"
install -Dm644 packaging/viberadio.svg "$STAGE/viberadio.svg"
install -Dm644 \
  flatpak/io.github.gishi1.VibeRadio.metainfo.xml \
  "$STAGE/io.github.gishi1.VibeRadio.metainfo.xml"

echo "==> packaging ${NAME}.tar.gz"
tar -C "$OUT" -czf "${OUT}/${NAME}.tar.gz" "$NAME"
(
  cd "$OUT"
  sha256sum "${NAME}.tar.gz" > SHA256SUMS
)

echo "==> artifacts"
ls -lh "$OUT"

if [ "$CREATE_TAG" -eq 1 ]; then
  if git rev-parse "v${VERSION}" >/dev/null 2>&1; then
    echo "==> tag v${VERSION} already exists"
  else
    git tag -a "v${VERSION}" -m "VibeRadio ${VERSION}"
    echo "==> created tag v${VERSION}"
  fi
  if [ "$PUSH_TAG" -eq 1 ]; then
    git push origin "v${VERSION}"
  fi
fi

echo "==> done"
