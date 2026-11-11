#!/usr/bin/env bash
# Build the Debian package in a clean Debian container and copy it to dist/.
# This avoids a NixOS-built binary that links to /nix/store.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if ! command -v docker >/dev/null 2>&1; then
    echo "docker is required; see docs/RELEASE.md for the manual build" >&2
    exit 1
fi

docker build -f packaging/Dockerfile.release -t viberadio-release .
mkdir -p artifacts
docker run --rm --user "$(id -u):$(id -g)" \
    -v "$ROOT/artifacts:/out" viberadio-release \
    cp -a /src/artifacts/. /out/
echo "artifacts copied to artifacts/"
