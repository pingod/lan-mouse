#!/bin/bash
# Sign the macOS app bundle with a stable local certificate.
#
# Why this exists: with ad-hoc signing (`codesign --sign -`) macOS anchors the
# TCC grant to the binary's cdhash. Every rebuild changes the cdhash, so the
# Accessibility / Input Monitoring grants are dropped and the user has to
# re-authorise each time. Signing with a fixed certificate makes the designated
# requirement point at that certificate instead, so grants survive rebuilds.
#
# One-time setup (needs an admin password, so it is not automated here):
#
#   cd /tmp/lmcert
#   security create-keychain -p lmsign signing.keychain-db
#   security set-keychain-settings -lut 21600 signing.keychain
#   security unlock-keychain -p lmsign signing.keychain
#   security import id.p12 -k signing.keychain -P lm \
#       -T /usr/bin/codesign -T /usr/bin/security
#   security set-key-partition-list -S apple-tool:,apple:,codesign: \
#       -s -k lmsign signing.keychain
#   security find-certificate -c "Lan Mouse Local Signing" -p \
#       signing.keychain > lanmouse.cer
#   sudo security add-trusted-cert -d -r trustRoot \
#       -k /Library/Keychains/System.keychain lanmouse.cer
#
# Usage: scripts/macos-sign.sh "/Applications/Lan Mouse.app"

set -euo pipefail

APP="${1:?usage: macos-sign.sh <path to .app>}"
KEYCHAIN="${LAN_MOUSE_SIGNING_KEYCHAIN:-$HOME/Library/Keychains/signing.keychain-db}"
KEYCHAIN_PASSWORD="${LAN_MOUSE_SIGNING_KEYCHAIN_PASSWORD:-lmsign}"
# Sign by certificate SHA-1, not by name: `codesign --sign <name>` requires the
# certificate to be trusted, while signing by hash works with a self-signed
# certificate that was never added to a trust store. The designated
# requirement still anchors to this certificate, which is all TCC needs.
IDENTITY="${LAN_MOUSE_SIGNING_IDENTITY:-8A3127DAB5EC34226D2EF12788DEA98DFE6338E4}"

if [ ! -f "$KEYCHAIN" ]; then
    echo "error: signing keychain not found at $KEYCHAIN" >&2
    echo "run the one-time setup documented at the top of this script" >&2
    exit 1
fi

# The keychain is locked on every new session; unlocking here keeps the build
# non-interactive. The password is local-only and never leaves this machine.
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"

if ! security find-certificate -a -c "Lan Mouse Local Signing" -Z "$KEYCHAIN" \
    | grep -qi "${IDENTITY//:/}"; then
    echo "error: certificate $IDENTITY not found in $KEYCHAIN" >&2
    exit 1
fi

# Sign inside-out: nested dylibs and frameworks first, the bundle last. A single
# `--deep` pass is deprecated and misses some nested code on current macOS.
find "$APP/Contents/Frameworks" -type f \( -name "*.dylib" -o -name "*.so" \) \
    -print0 2>/dev/null |
    while IFS= read -r -d '' lib; do
        codesign --force --sign "$IDENTITY" --keychain "$KEYCHAIN" \
            --timestamp=none "$lib"
    done

find "$APP/Contents/Frameworks" -mindepth 1 -maxdepth 1 -type d -name "*.framework" \
    -print0 2>/dev/null |
    while IFS= read -r -d '' fw; do
        codesign --force --sign "$IDENTITY" --keychain "$KEYCHAIN" \
            --timestamp=none "$fw"
    done

codesign --force --sign "$IDENTITY" --keychain "$KEYCHAIN" --timestamp=none \
    --options runtime "$APP/Contents/MacOS/lan-mouse"

codesign --force --sign "$IDENTITY" --keychain "$KEYCHAIN" --timestamp=none \
    --options runtime "$APP"

codesign --verify --deep --strict "$APP"
echo "signed with '$IDENTITY':"
codesign -d -r- "$APP" 2>&1 | tail -1
