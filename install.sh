#!/bin/sh
# Installs Hoardini for the current user (no root needed):
#   ~/.local/bin/{hoardini-app,hoardini,hoardini-host}, the app menu entry and icon,
#   and the Firefox extension files in ~/.local/share/hoardini.
# Then starts it once, which connects Firefox to it and shows how to add the extension.
#   ./install.sh              install or update
#   ./install.sh --uninstall  remove Hoardini (your downloaded files are never touched)
set -eu

bin="$HOME/.local/bin"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
share="$data/hoardini"
manifests="$HOME/.mozilla/native-messaging-hosts/com.hoardini.host.json $HOME/.librewolf/native-messaging-hosts/com.hoardini.host.json $HOME/.waterfox/native-messaging-hosts/com.hoardini.host.json"

# Ask a running copy (only this user's) to pause its downloads and quit, so the programs can be replaced.
if pkill -TERM -x -u "$(id -u)" hoardini-app 2>/dev/null; then
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        pgrep -x -u "$(id -u)" hoardini-app >/dev/null || break
        sleep 0.5
    done
fi

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin/hoardini-app" "$bin/hoardini" "$bin/hoardini-host" \
        "$data/applications/hoardini.desktop" "$data/icons/hicolor/256x256/apps/hoardini.png" "$share/hoardini.png"
    # shellcheck disable=SC2086
    rm -f $manifests
    rm -rf "$share/extension" "$share/hoardini.xpi"
    update-desktop-database "$data/applications" 2>/dev/null || true
    echo "Hoardini has been removed. Remove the extension in Firefox (about:addons)."
    echo "Your download list and settings are in $share if you want to delete them too."
    exit 0
fi

# The programs next to this script (the release archive), or the build output in the source tree.
here=$(cd "$(dirname "$0")" && pwd)
if [ -x "$here/hoardini-app" ]; then
    src="$here"
elif [ -x "$here/target/release/hoardini-app" ]; then
    src="$here/target/release"
else
    echo "hoardini-app not found. Build it first with: cargo build --release" >&2
    exit 1
fi

mkdir -p "$bin" "$share" "$data/applications" "$data/icons/hicolor/256x256/apps"
chmod 700 "$share"
for p in hoardini-app hoardini hoardini-host; do
    # Copy then rename, so a running program is never overwritten in place.
    cp "$src/$p" "$bin/$p.new"
    chmod 755 "$bin/$p.new"
    mv -f "$bin/$p.new" "$bin/$p"
done

rm -rf "$share/extension.new"
cp -r "$here/extension" "$share/extension.new"
rm -rf "$share/extension"
mv "$share/extension.new" "$share/extension"
if [ -f "$here/hoardini.xpi" ]; then
    cp "$here/hoardini.xpi" "$share/hoardini.xpi"
elif [ -f "$here/dist/hoardini.xpi" ]; then
    cp "$here/dist/hoardini.xpi" "$share/hoardini.xpi"
fi

icon="$here/hoardini-256.png"
[ -f "$icon" ] || icon="$here/packaging/icons/hoardini-256.png"
cp "$icon" "$share/hoardini.png"
cp "$icon" "$data/icons/hicolor/256x256/apps/hoardini.png"
desktop="$here/hoardini.desktop"
[ -f "$desktop" ] || desktop="$here/packaging/hoardini.desktop"
# The icon by full path: menus don't always look in ~/.local/share/icons (it has no index.theme).
sed -e "s|^Exec=.*|Exec=\"$bin/hoardini-app\"|" -e "s|^Icon=.*|Icon=$share/hoardini.png|" "$desktop" > "$data/applications/hoardini.desktop"
update-desktop-database "$data/applications" 2>/dev/null || true
gtk-update-icon-cache -q "$data/icons/hicolor" 2>/dev/null || true

command -v ffmpeg >/dev/null || echo "Note: converting to MP4 and saving videos with separate sound need ffmpeg (sudo apt install ffmpeg)."

# Starting it registers the Firefox connection (pointing at $bin/hoardini-host).
nohup "$bin/hoardini-app" >/dev/null 2>&1 &
echo "Hoardini is installed and running. Find it in the app menu as Hoardini."
