#!/bin/bash
# Install a lan-mouse macOS release zip into /Applications and restart it.
#
# Why this exists: a release built by CI is signed ad hoc, and macOS anchors
# the Accessibility / Input Monitoring grants to the binary's cdhash for ad-hoc
# signatures. Replacing `/Applications/Lan Mouse.app` that way therefore drops
# the grants and the user has to re-authorise after every update. Signing the
# installed bundle with the stable local certificate (`macos-sign.sh`) makes the
# designated requirement point at the certificate instead, so the grants survive
# updates.
#
# Usage: scripts/mac-deploy.sh ~/Downloads/lan-mouse-macos-arm64.zip
#
# One-time setup of the signing identity: see the header of macos-sign.sh.

set -euo pipefail

ZIP="${1:?usage: mac-deploy.sh <lan-mouse-macos-arm64.zip>}"
APP="/Applications/Lan Mouse.app"
LABEL="de.feschber.LanMouse"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
SIGN="$(cd "$(dirname "$0")" && pwd)/macos-sign.sh"

[ -f "$ZIP" ] || { echo "error: $ZIP does not exist" >&2; exit 1; }
[ -x "$SIGN" ] || { echo "error: $SIGN is missing" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

ditto -x -k "$ZIP" "$STAGE"
[ -d "$STAGE/Lan Mouse.app" ] || {
    echo "error: the archive does not contain 'Lan Mouse.app'" >&2
    exit 1
}
echo "staged: $("$STAGE/Lan Mouse.app/Contents/MacOS/lan-mouse" --version | tr '\n' ' ')"

# The LaunchAgent restarts the app on exit, so stop it before touching the
# bundle; otherwise it comes back in the middle of the replacement.
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
pkill -f "[L]an Mouse.app" 2>/dev/null || true
sleep 2

if [ -d "$APP" ]; then
    BACKUP="/tmp/Lan Mouse.app.old-$(date +%Y%m%d-%H%M%S)"
    mv "$APP" "$BACKUP"
    echo "previous bundle kept at $BACKUP"
fi

ditto "$STAGE/Lan Mouse.app" "$APP"
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

# Re-sign before the first launch: the TCC grant is matched against the
# signature of the running binary.
"$SIGN" "$APP"

# Start through the LaunchAgent when there is one: it owns the log files
# (`StandardErrorPath`), and launching with `open -a` instead would leave a
# second, unmanaged instance writing to /dev/null.
if [ -f "$PLIST" ]; then
    launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>/dev/null || open -a "$APP"
else
    open -a "$APP"
fi
sleep 3

echo "installed: $("$APP/Contents/MacOS/lan-mouse" --version | tr '\n' ' ')"
codesign -d -r- "$APP" 2>&1 | tail -1
