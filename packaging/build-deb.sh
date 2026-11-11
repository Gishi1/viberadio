#!/usr/bin/env bash
# Build a Debian package from the release binary.
#
# Build this on Debian/Ubuntu (or inside packaging/Dockerfile.release), not on
# NixOS: a NixOS build links to /nix/store and the package will not run on a
# normal distribution. The script warns if it detects that.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [[ -z "$VERSION" ]]; then
    echo "could not read the version from Cargo.toml" >&2
    exit 1
fi
ARCH="$(dpkg --print-architecture)"

cargo build --release

if ldd target/release/viberadio | grep -q /nix/store; then
    echo "warning: the binary links to /nix/store; build the release in" >&2
    echo "packaging/Dockerfile.release for a portable package" >&2
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
# mktemp creates the staging root as 0700; dpkg-deb would package that mode for
# the archive root. The installed files live under /usr, but the root entry must
# still be a normal 0755 directory.
chmod 755 "$STAGE"

install -Dm755 target/release/viberadio "$STAGE/usr/bin/viberadio"
install -Dm644 packaging/viberadio.desktop \
    "$STAGE/usr/share/applications/viberadio.desktop"
install -Dm644 packaging/viberadio.svg \
    "$STAGE/usr/share/icons/hicolor/scalable/apps/viberadio.svg"
install -Dm644 README.md "$STAGE/usr/share/doc/viberadio/README.md"

mkdir -p "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: viberadio
Version: $VERSION
Section: sound
Priority: optional
Architecture: $ARCH
Maintainer: VibeRadio maintainers <noreply@example.invalid>
Depends: libasound2 | libasound2t64, libc6, libx11-6, libxkbcommon0, libxkbcommon-x11-0 | libxkbcommon-x11-0t64, libgl1, libwayland-client0, libxcb1, libxrandr2, libxi6, libxcursor1, libegl1
Description: Native internet-radio client
 A Rust/egui internet-radio client with multi-stream playback, recording,
 schedules, metadata history and Dynamic Boost.
EOF

mkdir -p artifacts
OUT="artifacts/viberadio_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$OUT"
echo "wrote $OUT"
