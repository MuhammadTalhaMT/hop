#!/bin/bash
# Download the newest hop.exe that CI built, into ~/Downloads/hop-exe/.
# Grab it from the PC over your network share, or copy it across however
# you normally move files.
# Usage: ./fetch-exe.sh
set -e
cd "$(dirname "$0")"
RID=$(gh run list --limit 1 --branch feat/hop-platform --json databaseId,conclusion \
      --jq '.[0] | select(.conclusion=="success") | .databaseId')
if [ -z "$RID" ]; then
  echo "the most recent CI run has not succeeded yet; check:"
  gh run list --limit 1
  exit 1
fi
rm -rf ~/Downloads/hop-exe
gh run download "$RID" -n hop-windows -D ~/Downloads/hop-exe
echo
echo "hop.exe is now at: ~/Downloads/hop-exe/hop.exe"
ls -la ~/Downloads/hop-exe/
