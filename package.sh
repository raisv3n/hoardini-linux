#!/bin/sh
# Builds the release binaries and packages them in dist/:
#   hoardini_<version>-1_amd64.deb     for Debian, Ubuntu and derivatives (needs cargo-deb)
#   Hoardini-linux-x86_64.tar.gz       the programs with install.sh, for any distribution
# If dist/hoardini.xpi (the Mozilla-signed extension, see scripts/sign-xpi.sh) exists, both include it,
# and the app's setup dialog installs it in Firefox with one click.
set -eu
cd "$(dirname "$0")"

signed=""
if [ -f dist/hoardini.xpi ]; then
    unzip -l dist/hoardini.xpi | grep -q 'META-INF/' || { echo "dist/hoardini.xpi isn't signed" >&2; exit 1; }
    signed=dist/hoardini.xpi
    mkdir -p target/xpi
    cp "$signed" target/xpi/hoardini.xpi
fi
find dist -maxdepth 1 -type f ! -name hoardini.xpi -delete 2>/dev/null || true

cargo build --locked --release
if [ -n "$signed" ]; then
    cargo deb -p hoardini-app --no-build --locked --variant signed --output dist/
else
    echo "Note: no signed dist/hoardini.xpi; packaging without it." >&2
    cargo deb -p hoardini-app --no-build --locked --output dist/
fi

stage=dist/Hoardini
rm -rf "$stage"
mkdir -p "$stage"
cp target/release/hoardini-app target/release/hoardini target/release/hoardini-host "$stage/"
cp -r extension "$stage/extension"
cp install.sh README.md LICENSE packaging/hoardini.desktop packaging/icons/hoardini-256.png "$stage/"
[ -n "$signed" ] && cp "$signed" "$stage/"
chmod 755 "$stage/hoardini-app" "$stage/hoardini" "$stage/hoardini-host" "$stage/install.sh"
tar -czf dist/Hoardini-linux-x86_64.tar.gz -C dist Hoardini
rm -rf "$stage"
ls -l dist
