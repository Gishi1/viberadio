#!/usr/bin/env bash
#
# Regenerate flatpak/cargo-sources.json from Cargo.lock.
#
# The Flatpak build has no network, so every crate is pinned as a source.
# Run this after changing dependencies.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT="${1:-flatpak/cargo-sources.json}"

if command -v flatpak-cargo-generator >/dev/null 2>&1; then
  flatpak-cargo-generator Cargo.lock -o "$OUT"
  exit 0
fi

GENERATOR="$(mktemp --suffix=.py)"
trap 'rm -f "$GENERATOR"' EXIT
curl -sSfL \
  https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py \
  -o "$GENERATOR"

if python3 -c "import aiohttp, tomlkit" >/dev/null 2>&1; then
  python3 "$GENERATOR" Cargo.lock -o "$OUT"
elif command -v nix-shell >/dev/null 2>&1; then
  nix-shell -p python3Packages.aiohttp python3Packages.tomlkit \
    --run "python3 '$GENERATOR' Cargo.lock -o '$OUT'"
else
  echo "The generator needs Python aiohttp and tomlkit." >&2
  echo "Install them, or run: nix-shell -p python3Packages.aiohttp python3Packages.tomlkit" >&2
  exit 1
fi
