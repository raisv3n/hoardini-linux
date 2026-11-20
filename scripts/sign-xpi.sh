#!/bin/sh
# Signs the extension through addons.mozilla.org as an unlisted ("On your own") add-on and saves
# the signed package as dist/hoardini.xpi. Unlisted add-ons aren't published on the store; Mozilla
# only reviews and signs them, usually within minutes.
#
# Needs Node.js (for Mozilla's web-ext tool) and AMO API credentials from
# https://addons.mozilla.org/developers/addon/api/key/ in the environment:
#   AMO_JWT_ISSUER=user:12345:67 AMO_JWT_SECRET=... ./scripts/sign-xpi.sh
# Each signed upload needs a new "version" in extension/manifest.json.
set -eu
cd "$(dirname "$0")/.."
: "${AMO_JWT_ISSUER:?set AMO_JWT_ISSUER (see the comment at the top of this script)}"
: "${AMO_JWT_SECRET:?set AMO_JWT_SECRET}"
command -v npx >/dev/null || { echo "npx not found: install Node.js (sudo apt install nodejs npm)" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# The credentials go through the environment (WEB_EXT_API_*), never the command line.
WEB_EXT_API_KEY="$AMO_JWT_ISSUER" WEB_EXT_API_SECRET="$AMO_JWT_SECRET" \
    npx --yes web-ext@8.10.0 sign --source-dir extension --channel unlisted --artifacts-dir "$work"
signed=$(find "$work" -name '*.xpi' | head -n1)
[ -n "$signed" ] || { echo "web-ext didn't produce a signed .xpi" >&2; exit 1; }
mkdir -p dist
cp "$signed" dist/hoardini.xpi
echo "dist/hoardini.xpi"
