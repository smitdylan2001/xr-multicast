#!/usr/bin/env bash
# Download scrcpy-server (Apache-2.0, the on-headset screen encoder) into vendor/ and
# verify its SHA-256. Prints the path. Does nothing if a verified copy is already there.
#
#   packaging/native/fetch-scrcpy-server.sh [dest]
set -euo pipefail

VERSION=5.0.1
SHA256=764eb6f79811d5211fe9df341120882ba9994c7a61b897d7bf3fb662e53bc536
URL="https://github.com/Genymobile/scrcpy/releases/download/v$VERSION/scrcpy-server-v$VERSION"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEST="${1:-$ROOT/vendor/scrcpy-server}"

sha() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

if [[ -f "$DEST" && "$(sha "$DEST")" == "$SHA256" ]]; then
  echo "$DEST"
  exit 0
fi
mkdir -p "$(dirname "$DEST")"
echo "downloading scrcpy-server v$VERSION" >&2
curl -fsSL --retry 3 -o "$DEST.part" "$URL"
got="$(sha "$DEST.part")"
if [[ "$got" != "$SHA256" ]]; then
  rm -f "$DEST.part"
  echo "scrcpy-server checksum mismatch: got $got, expected $SHA256" >&2
  exit 1
fi
mv "$DEST.part" "$DEST"
echo "$VERSION" > "$(dirname "$DEST")/scrcpy-server.version"
echo "$DEST"
