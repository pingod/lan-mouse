#!/bin/bash
# Build on the Mac and install the result into the existing app bundle.
#
# The Mac builds with Homebrew's GTK (no CI round trip), so a change is live in
# about a minute:
#
#     scripts/mac-install.sh
#
# Only the executable inside `/Applications/Lan Mouse.app` is replaced; the
# bundle keeps its frameworks and, because the bundle is re-signed with the local
# certificate afterwards, keeps its Accessibility grant (ad-hoc signed bundles
# lose the grant on every update).
#
# `.git` is synced so shadow-rs can stamp the commit into `--version`.
set -euo pipefail

HOST="${LM_MAC_HOST:-pavia@192.168.1.5}"
REMOTE="${LM_MAC_REMOTE:-githome/lan-mouse}"
SRC="$(cd "$(dirname "$0")/.." && pwd)"
APP="/Applications/Lan Mouse.app"
PLIST="Library/LaunchAgents/de.feschber.LanMouse.plist"

rsync -az --delete --exclude 'target/' --exclude 'examples/' "$SRC/" "$HOST:$REMOTE/"
rsync -az --delete "$SRC/.git/" "$HOST:$REMOTE/.git/"

ssh "$HOST" bash -s <<EOF
set -euo pipefail
cd "$REMOTE"
export PATH=/opt/homebrew/bin:\$PATH PKG_CONFIG_PATH=/opt/homebrew/lib/pkgconfig
cargo build --release --all-features 2>&1 | tail -1
./target/release/lan-mouse --version | tr '\n' ' '; echo

launchctl bootout "gui/\$(id -u)/de.feschber.LanMouse" 2>/dev/null || true
pkill -f "[L]an Mouse.app" 2>/dev/null || true
sleep 1

install -m 755 target/release/lan-mouse "$APP/Contents/MacOS/lan-mouse"
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
bash scripts/macos-sign.sh "$APP" 2>&1 | tail -1

launchctl bootstrap "gui/\$(id -u)" ~/"$PLIST" 2>/dev/null || open -a "$APP"
sleep 2
pgrep -fl "Lan Mouse.app" | head -2
EOF
