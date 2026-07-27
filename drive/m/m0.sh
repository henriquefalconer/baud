#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/m/m0.sh — M0 drive script: server + CLI bootstrap
#
# Validates:
#   baud keys show
#   baud server status
#   baud server logs
#   baud doctor

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Ensure the workspace is built
echo "==> Building workspace..."
cargo build --manifest-path "$REPO_ROOT/Cargo.toml" 2>&1

BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE=$(mktemp -t baud-m0-XXXXXX.sqlite)
# Windows/git-bash: sqlite:// URIs need a native Windows path (posix /tmp/... is not
# understood by a plain win32 binary); cygpath -m gives a forward-slash Windows path.
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" 2>/dev/null || true
}
trap cleanup EXIT

# Start the server
echo "==> Starting baud-server..."
pkill -f "baud-server" 2>/dev/null || true; sleep 0.2
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" "$BAUD_SERVER_BIN" &
SERVER_PID=$!
sleep 1  # Give it a moment to start

echo "==> baud server status"
"$BAUD" server status --json

echo "==> baud server logs"
"$BAUD" server logs --json

echo "==> baud keys show"
"$BAUD" keys show --json

echo "==> baud doctor (may fail if sops/age not installed)"
"$BAUD" doctor --json || true

echo "==> baud run ls"
"$BAUD" run ls --json

echo "==> baud budget"
"$BAUD" budget --json

echo ""
echo "==> M0 PASS"
