#!/bin/sh
# Packs extension/ into dist/hoardini-unsigned.xpi (a zip with manifest.json at its root).
# Firefox only keeps add-ons signed by Mozilla: sign this one with scripts/sign-xpi.sh,
# or upload it at https://addons.mozilla.org/developers/addon/submit/distribution
# ("On your own"), download the signed file and save it as dist/hoardini.xpi.
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist
out="$PWD/dist/hoardini-unsigned.xpi"
rm -f "$out"
# Files in a fixed order, so the package lists them the same way every time.
(cd extension && find . -type f ! -name '.*' | LC_ALL=C sort | TZ=UTC zip -X -q -D "$out" -@)
echo "$out"
