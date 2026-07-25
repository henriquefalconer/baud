#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/m10.sh — M10 drive script: exercise GET /shell-into/{run_id}/{node_id} and
# `baud shell-into` end-to-end over a real WebSocket against a real baud-server process on real
# /dev/kvm.
#
# specs/baud-snapshot.md §5's "restore into a live shell" — todo.md's own "Not yet done" note
# tracked this as needing "a genuinely new axum ws-feature route plus a bidirectional Multiverse
# session, universe-by-ID deserialization, CLI subcommand" (crate-level restore/PTY primitives
# already existed and passed via `shell_into_universe_resumes`, but nothing served them over HTTP
# or a CLI command before this).
#
#   M10.1 POST /run/kvm/branch in "persist-only" mode (empty branch_tapes_hex + persist_run_id) —
#         boots shell-guest (which never halts and never calls MARK_BRANCH, so it had no other way
#         to reach the SnapshotStore via this route), persists the post-boot universe, forks zero
#         branches.
#   M10.2 `baud shell-into <run_id> <node_id> --input-hex <hex of "hi\r"> --json` — a scripted
#         round trip: the guest must print its "$ " prompt, echo the queued input, and re-prompt,
#         exactly matching `shell_into_universe_resumes`'s own crate-level assertion.
#   M10.3 A second, independent `baud shell-into` call against the *same* persisted node produces
#         the identical transcript — restoring into a live shell is a pure function of the
#         persisted universe, not a one-shot resource.
#   M10.4 Error handling: `baud shell-into` against an unknown run_id/node_id reports an error
#         (via the WebSocket's own first message) instead of hanging or crashing the server.

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-m10-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" 2>/dev/null || true
}
trap cleanup EXIT

log() { echo "[m10] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

SHELL_GUEST_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/shell-guest/bzImage"
[[ -f "$SHELL_GUEST_KERNEL" ]] || fail "fixture kernel missing: $SHELL_GUEST_KERNEL"

# ---------------------------------------------------------------------------
# Build + start baud-server, build baud CLI
# ---------------------------------------------------------------------------
log "Building baud-server and baud..."
cargo build -q --bin baud-server --bin baud 2>&1

log "Starting baud-server (DB: $DB_FILE)..."
pkill -f "baud-server" 2>/dev/null || true; sleep 0.2
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_LOG=warn "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for i in $(seq 1 30); do
    if curl -sf http://127.0.0.1:7734/health > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf http://127.0.0.1:7734/health > /dev/null || fail "baud-server did not start"
pass "baud-server is running (PID $SERVER_PID)"

SRV="http://127.0.0.1:7734"

# ---------------------------------------------------------------------------
# M10.1 — POST /run/kvm/branch, persist-only mode
# ---------------------------------------------------------------------------
log "--- M10.1: POST /run/kvm/branch — persist-only mode, shell-guest ---"
RUN_ID="m10-shell-into-$$"
PERSIST=$(curl -sf -X POST "$SRV/run/kvm/branch" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$SHELL_GUEST_KERNEL\", \"persist_run_id\": \"$RUN_ID\"}")
PERSIST_OK=$(echo "$PERSIST" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$PERSIST_OK" == "True" ]] || fail "M10.1: /run/kvm/branch (persist-only) returned ok!=true: $PERSIST"
BRANCH_COUNT=$(echo "$PERSIST" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['branches']))")
[[ "$BRANCH_COUNT" == "0" ]] || fail "M10.1: expected zero forked branches, got $BRANCH_COUNT"
NODE_ID=$(echo "$PERSIST" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['node_id'])")
PERSISTED_RUN_ID=$(echo "$PERSIST" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['run_id'])")
[[ -n "$NODE_ID" && "$NODE_ID" != "None" ]] || fail "M10.1: expected a persisted node_id: $PERSIST"
[[ "$PERSISTED_RUN_ID" == "$RUN_ID" ]] || fail "M10.1: persisted.run_id mismatch: $PERSISTED_RUN_ID != $RUN_ID"
pass "M10.1: persist-only /run/kvm/branch persisted node_id=$NODE_ID under run_id=$RUN_ID, forked zero branches"

# ---------------------------------------------------------------------------
# M10.2 — baud shell-into, scripted round trip
# ---------------------------------------------------------------------------
log "--- M10.2: baud shell-into --input-hex — scripted round trip ---"
# "hi\r" hex-encoded: 68 69 0d
SHELL1=$("$BAUD" shell-into "$RUN_ID" "$NODE_ID" --input-hex 68690d --json)
SHELL1_OK=$(echo "$SHELL1" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$SHELL1_OK" == "True" ]] || fail "M10.2: baud shell-into returned ok!=true: $SHELL1"
SHELL1_HEX=$(echo "$SHELL1" | python3 -c "import sys,json; print(json.load(sys.stdin)['output_hex'])")
# Expected transcript: "$ " (prompt) + "hi" (echoed) + "\n$ " (newline + re-prompt) = "$ hi\n$ "
EXPECTED_HEX=$(python3 -c "print(b'\$ hi\n\$ '.hex())")
[[ "$SHELL1_HEX" == "$EXPECTED_HEX" ]] || fail "M10.2: expected transcript $EXPECTED_HEX, got $SHELL1_HEX"
pass "M10.2: shell-into echoed queued input and re-prompted (transcript: \$ hi<LF>\$ )"

# ---------------------------------------------------------------------------
# M10.3 — a second, independent shell-into call against the same node reproduces byte-identically
# ---------------------------------------------------------------------------
log "--- M10.3: a second independent shell-into call reproduces byte-identically ---"
SHELL2=$("$BAUD" shell-into "$RUN_ID" "$NODE_ID" --input-hex 68690d --json)
SHELL2_HEX=$(echo "$SHELL2" | python3 -c "import sys,json; print(json.load(sys.stdin)['output_hex'])")
[[ "$SHELL2_HEX" == "$SHELL1_HEX" ]] || fail "M10.3: second shell-into call diverged: $SHELL2_HEX != $SHELL1_HEX"
pass "M10.3: restoring the same persisted node twice produces byte-identical transcripts"

# ---------------------------------------------------------------------------
# M10.4 — error handling: unknown run_id/node_id
# ---------------------------------------------------------------------------
log "--- M10.4: baud shell-into against an unknown run_id/node_id ---"
BOGUS_NODE="$(printf '0%.0s' {1..64})"
set +e
SHELL_ERR=$("$BAUD" shell-into "no-such-run-$$" "$BOGUS_NODE" --input-hex 68690d --json --idle-timeout-ms 1000)
set -e
SHELL_ERR_OUTPUT_HEX=$(echo "$SHELL_ERR" | python3 -c "import sys,json; print(json.load(sys.stdin).get('output_hex',''))" 2>/dev/null || echo "")
if [[ -n "$SHELL_ERR_OUTPUT_HEX" ]]; then
    SHELL_ERR_TEXT=$(python3 -c "import sys; print(bytes.fromhex('$SHELL_ERR_OUTPUT_HEX').decode('utf-8', 'replace'))")
    [[ "$SHELL_ERR_TEXT" == *"shell-into"* ]] || fail "M10.4: expected an error message in the transcript, got: $SHELL_ERR_TEXT"
    pass "M10.4: unknown run_id/node_id reported an in-band error, no hang/crash ($SHELL_ERR_TEXT)"
else
    fail "M10.4: baud shell-into produced no parseable output for an unknown run_id/node_id: $SHELL_ERR"
fi

# The server must still be alive and healthy after all of the above.
curl -sf http://127.0.0.1:7734/health > /dev/null || fail "M10.4: baud-server is no longer healthy after error case"
pass "M10.4: baud-server remained healthy throughout"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "==========================================="
echo "ALL M10 CHECKS PASSED"
echo "==========================================="
echo ""
echo "GET /shell-into/{run_id}/{node_id} and 'baud shell-into' exercised end-to-end over a real"
echo "WebSocket against a real baud-server process on real /dev/kvm:"
echo "  persist-only /run/kvm/branch — reaches the SnapshotStore for a guest with no MARK_BRANCH"
echo "  baud shell-into --input-hex  — scripted send-then-collect round trip, byte-exact echo"
echo "  repeatability                — restoring the same node twice reproduces byte-identically"
echo "  error handling               — unknown run_id/node_id reports in-band, no hang/crash"
