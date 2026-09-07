#!/bin/bash
# Rebuild hop on this Mac, sign it with a stable identity, and put it in place.
# Usage: ./update-mac.sh
set -e
cd "$(dirname "$0")"
source "$HOME/.cargo/env"

# Signing with a real certificate rather than the default ad-hoc signature
# is what lets macOS recognise each rebuild as the SAME program. Ad-hoc
# signatures are keyed on the binary's contents, so every build looked like
# a brand new app and Accessibility permission had to be granted again.
# Your own Apple Development certificate, as a SHA-1 fingerprint. Find
# yours with:
#   security find-identity -v -p codesigning
# Put it in .signing-identity next to this script (untracked), or set
# HOP_SIGN_IDENTITY in your shell.
if [ -z "$HOP_SIGN_IDENTITY" ] && [ -f .signing-identity ]; then
  HOP_SIGN_IDENTITY=$(tr -d '[:space:]' < .signing-identity)
fi
IDENTITY="${HOP_SIGN_IDENTITY:?no signing identity: put one in .signing-identity or set HOP_SIGN_IDENTITY (see: security find-identity -v -p codesigning)}"

echo "building..."
cargo build --release -p hop

pkill -f "hop run" 2>/dev/null || true
cp target/release/hop ~/Downloads/hop
chmod +x ~/Downloads/hop

echo "signing as com.talha.hop..."
codesign --force --sign "$IDENTITY" --identifier "com.talha.hop" ~/Downloads/hop

echo
echo "updated and signed: ~/Downloads/hop"
codesign -dv ~/Downloads/hop 2>&1 | grep -E "Identifier|TeamIdentifier"
