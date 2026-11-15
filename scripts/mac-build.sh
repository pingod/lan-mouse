#!/bin/bash
# Sync source to the Mac build tree and rebuild incrementally.
#
# Keeps `target/` and `.git/` in place: a full rebuild costs ~90 s of dependency
# compilation, while syncing only the sources makes a change take ~10 s. `.git`
# stays so shadow-rs can stamp the commit.
set -euo pipefail

HOST="${LM_MAC_HOST:-pavia@192.168.1.5}"
REMOTE=/tmp/lm-build
SRC=/tmp/lm-src

# Files that define the build; everything else is copied as-is. `target/` stays
# for incremental compilation; `.git` is refreshed separately below because
# syncing it wholesale is slow and shadow-rs only reads the commit.
rsync -az --delete \
    --exclude 'target/' \
    --exclude '.git/' \
    --exclude 'examples/' \
    "$SRC/" "$HOST:$REMOTE/"

# Keep the remote repository in step so the build stamp matches this tree.
# shadow-rs reads `.git` directly and has no environment override, so the
# metadata has to be present rather than passed in.
rsync -az --delete "$SRC/.git/" "$HOST:$REMOTE/.git/"

# Rebuild. `--all-features` matches what ships.
ssh "$HOST" "cd $REMOTE && export PATH=/opt/homebrew/bin:\$PATH PKG_CONFIG_PATH=/opt/homebrew/lib/pkgconfig && cargo build --release --all-features 2>&1 | tail -5"

# Report the stamp so a stale build is obvious.
ssh "$HOST" "cd $REMOTE && grep -h 'SHORT_COMMIT :&str' target/release/build/lan-mouse-*/out/shadow.rs 2>/dev/null | head -1"
