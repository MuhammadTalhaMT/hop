#!/bin/bash
# Rebuild hop on this Mac and drop the binary in place.
# Usage: ./update-mac.sh
set -e
cd "$(dirname "$0")"
source "$HOME/.cargo/env"
echo "building..."
cargo build --release -p hop
pkill -f "hop run" 2>/dev/null || true
cp target/release/hop ~/Downloads/hop
chmod +x ~/Downloads/hop
echo "updated ~/Downloads/hop"
echo
echo "NOTE: macOS ties Accessibility permission to the exact binary, so a"
echo "rebuild needs the permission re-granting until we set up stable signing."
