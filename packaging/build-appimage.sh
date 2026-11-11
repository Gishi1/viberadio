#!/usr/bin/env bash
# Build an AppImage. Requires linuxdeploy and appimagetool on PATH.
#
# linuxdeploy bundles the libraries it can find. Build on the oldest
# distribution you intend to support: an AppImage built against a newer glibc
# will not start on an older one.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

for tool in linuxdeploy appimagetool; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "$tool is required (https://github.com/linuxdeploy/linuxdeploy and" >&2
        echo "https://github.com/AppImage/AppImageKit/releases)" >&2
        exit 1
    fi
done

cargo build --release

rm -rf AppDir
mkdir -p AppDir/usr/bin \
    AppDir/usr/share/applications \
    AppDir/usr/share/icons/hicolor/scalable/apps
install -Dm755 target/release/viberadio AppDir/usr/bin/viberadio
install -Dm644 packaging/viberadio.desktop \
    AppDir/usr/share/applications/viberadio.desktop
install -Dm644 packaging/viberadio.svg \
    AppDir/usr/share/icons/hicolor/scalable/apps/viberadio.svg

ARCH=x86_64 linuxdeploy \
    --appdir AppDir \
    --executable AppDir/usr/bin/viberadio \
    --desktop-file AppDir/usr/share/applications/viberadio.desktop \
    --icon-file AppDir/usr/share/icons/hicolor/scalable/apps/viberadio.svg

mkdir -p artifacts
ARCH=x86_64 appimagetool AppDir artifacts/VibeRadio-x86_64.AppImage
echo "wrote artifacts/VibeRadio-x86_64.AppImage"
