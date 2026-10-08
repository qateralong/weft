#!/bin/sh
# Removes Weft: sudo /usr/local/share/weft/uninstall.sh
set -e
launchctl bootout system/org.weft.weftd 2> /dev/null || true
rm -f /Library/LaunchDaemons/org.weft.weftd.plist /Library/LaunchAgents/org.weft.gui.plist
rm -f /usr/local/bin/weft /usr/local/bin/weftd
rm -rf /Applications/Weft.app /usr/local/share/weft
pkgutil --forget io.github.qateralong.weft > /dev/null 2>&1 || true
dseditgroup -o delete weft 2> /dev/null || true
echo "Weft removed. Device keys stay in /Library/Application Support/Weft."
