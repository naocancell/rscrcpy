#!/bin/sh
# Downloads the official scrcpy-server release and verifies its SHA-256.
# Usage: scripts/fetch-server.sh [OUTPUT]   (default: ./scrcpy-server at the repo root)
set -eu
VERSION=5.0.1
SHA256=764eb6f79811d5211fe9df341120882ba9994c7a61b897d7bf3fb662e53bc536
out=${1:-"$(dirname "$0")/../scrcpy-server"}
url="https://github.com/Genymobile/scrcpy/releases/download/v$VERSION/scrcpy-server-v$VERSION"

curl -fL --retry 3 -o "$out.part" "$url"
if command -v sha256sum >/dev/null 2>&1; then
    sum=$(sha256sum "$out.part" | cut -d' ' -f1)
else
    sum=$(shasum -a 256 "$out.part" | cut -d' ' -f1)
fi
if [ "$sum" != "$SHA256" ]; then
    rm -f "$out.part"
    echo "checksum mismatch: got $sum, expected $SHA256" >&2
    exit 1
fi
mv "$out.part" "$out"
echo "scrcpy-server v$VERSION -> $out (checksum OK)"
