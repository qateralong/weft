#!/usr/bin/env bash
# Builds an unsigned Weft installer package: Weft.app, weft and weftd, launchd jobs.
#   build-pkg.sh VERSION OUTPUT.pkg   (binaries from $BIN, default target/release)
set -euo pipefail

version=$1
output=$2
bin=${BIN:-target/release}
here=$(cd "$(dirname "$0")" && pwd)
icon=$here/../../crates/weft-gui/icons/icon.png
work=$(mktemp -d)
root=$work/root
app=$root/Applications/Weft.app/Contents

mkdir -p "$app/MacOS" "$app/Resources" "$root/usr/local/bin" "$root/usr/local/share/weft" \
    "$root/Library/LaunchDaemons" "$root/Library/LaunchAgents" "$work/scripts"
cp "$bin/weft-gui" "$app/MacOS/weft-gui"
cp "$bin/weft" "$bin/weftd" "$root/usr/local/bin/"
cp "$here/uninstall.sh" "$root/usr/local/share/weft/uninstall.sh"
cp "$here/org.weft.weftd.plist" "$root/Library/LaunchDaemons/"
cp "$here/org.weft.gui.plist" "$root/Library/LaunchAgents/"
cp "$here/postinstall" "$work/scripts/postinstall"
chmod 755 "$work/scripts/postinstall" "$root/usr/local/share/weft/uninstall.sh"

iconset=$work/weft.iconset
mkdir "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$icon" --out "$iconset/icon_${size}x${size}.png" > /dev/null
    sips -z $((size * 2)) $((size * 2)) "$icon" --out "$iconset/icon_${size}x${size}@2x.png" > /dev/null
done
iconutil -c icns "$iconset" -o "$app/Resources/weft.icns"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Info.plist"

pkgbuild --root "$root" --scripts "$work/scripts" --identifier io.github.qateralong.weft \
    --version "$version" --install-location / "$output"
rm -rf "$work"
