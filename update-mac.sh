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
IDENTITY="${HOP_SIGN_IDENTITY:-85A0610DC10063F62081B4BF147C105DB36A7D4F}"

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
