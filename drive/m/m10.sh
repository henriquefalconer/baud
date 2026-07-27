#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/m/m10.sh — M10 drive script: exercise GET /shell-into/{run_id}/{node_id} and
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

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-m10-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"
SNAP_ROOT="$(mktemp -d -t baud-m10-snap-XXXXXX)"

# Ephemeral port + per-script snapshot store, so this script can run concurrently with any other
# drive/*.sh (each server gets its own port, its own SQLite file and its own SnapshotStore root).
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" 2>/dev/null || true
    rm -rf "$SNAP_ROOT" 2>/dev/null || true
}
trap cleanup EXIT
# bash does NOT run an EXIT trap when it dies from an untrapped signal, so `trap cleanup EXIT`
# alone leaks this script's baud-server, temp DB and snapshot dir whenever the script is
# interrupted -- Ctrl-C, or drive/gate.sh reaping its pool. Re-raising through `exit` makes the
# EXIT trap fire, so cleanup() runs on every exit path. (This is how 21 stray temp SQLite files
# and two orphaned servers survived a killed gate run.)
trap 'exit 130' INT
trap 'exit 143' TERM

log() { echo "[m10] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

SHELL_GUEST_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/shell-guest/bzImage"
[[ -f "$SHELL_GUEST_KERNEL" ]] || fail "fixture kernel missing: $SHELL_GUEST_KERNEL"

# ---------------------------------------------------------------------------
# Build + start baud-server, build baud CLI
# ---------------------------------------------------------------------------
log "Building baud-server and baud..."
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q --bin baud-server --bin baud 2>&1
fi

log "Starting baud-server (DB: $DB_FILE, port: $BAUD_PORT)..."
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$BAUD_PORT" \
    BAUD_SNAPSHOT_STORE="$SNAP_ROOT" BAUD_LOG=warn "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for i in $(seq 1 60); do
    if curl -sf "$SRV/health" > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf "$SRV/health" > /dev/null || fail "baud-server did not start"
pass "baud-server is running (PID $SERVER_PID)"

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
#
# --first-byte-timeout-ms 15000 (vs the CLI's 10000ms default): `baud shell-into --input-hex`
# used to collect guest output until one shared --idle-timeout-ms passed with *nothing at all*
# received, so that one number doubled as a first-byte deadline too. Restoring the universe and
# stepping the guest far enough to emit its "$ " prompt takes well under 2s on an idle box, but
# reliably exceeds a short idle timeout when other drive scripts are booting their own guests
# concurrently — the collector then returned an empty transcript and M10.2/M10.3 failed
# spuriously. Measured directly on this host with three sibling drive scripts running: 2000ms gave
# an empty output_hex 3/3 times, 8000ms gave the exact expected transcript 3/3 times. shell_into.rs
# now splits this into --first-byte-timeout-ms ("guest hasn't started yet because restore is slow
# under load", used only while output is still empty) and --idle-timeout-ms ("guest stopped
# talking", used once output has started and left at its fast 2000ms default here). 15000 keeps
# headroom beyond the measured 8000ms if drive/gate.sh is ever run wider, since the needed margin
# scales with how many guests are booting at once.
#
# Raising this is close to free: it only lengthens how long we are willing to WAIT for the first
# byte. The assertion below is still an exact, byte-for-byte transcript match, so a genuinely
# broken shell-into still fails — it just fails later. The only thing a generous value can hide is
# a latency regression in shell-into itself.
SHELL1=$("$BAUD" shell-into "$RUN_ID" "$NODE_ID" --input-hex 68690d --first-byte-timeout-ms 15000 --json)
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
SHELL2=$("$BAUD" shell-into "$RUN_ID" "$NODE_ID" --input-hex 68690d --first-byte-timeout-ms 15000 --json)
SHELL2_HEX=$(echo "$SHELL2" | python3 -c "import sys,json; print(json.load(sys.stdin)['output_hex'])")
[[ "$SHELL2_HEX" == "$SHELL1_HEX" ]] || fail "M10.3: second shell-into call diverged: $SHELL2_HEX != $SHELL1_HEX"
pass "M10.3: restoring the same persisted node twice produces byte-identical transcripts"

# ---------------------------------------------------------------------------
# M10.4 — error handling: unknown run_id/node_id
# ---------------------------------------------------------------------------
log "--- M10.4: baud shell-into against an unknown run_id/node_id ---"
BOGUS_NODE="$(printf '0%.0s' {1..64})"
set +e
SHELL_ERR=$("$BAUD" shell-into "no-such-run-$$" "$BOGUS_NODE" --input-hex 68690d --json --first-byte-timeout-ms 1000 --idle-timeout-ms 1000)
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
curl -sf "$SRV/health" > /dev/null || fail "M10.4: baud-server is no longer healthy after error case"
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
