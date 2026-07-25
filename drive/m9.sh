#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/m9.sh — M9 drive script: exercise POST /run/kvm, /run/kvm/branch, /run/kvm/resume
# end-to-end over real HTTP against a real baud-server process on real /dev/kvm.
#
# todo.md §14 flagged this as a real gap after the ninth-brick increment: these three routes
# (crates/baud-server/src/routes/run_kvm.rs) were covered only by that module's own #[cfg(test)]
# unit tests calling their Rust functions directly — no drive/*.sh script ever sent them real HTTP
# requests. This script closes that gap.
#
#   M9.1  POST /run/kvm boots hello-guest twice — ram_hash identical across both boots (HTTP-level
#         double_boot_memory_identical)
#   M9.2  POST /run/kvm/branch (fixed-tape, tape-echo-guest) forks independent branches — each
#         echoes exactly its own suffix, no mark_branch_step (tape-echo-guest never calls it)
#   M9.3  POST /run/kvm/branch (fixed-tape, mark-branch-guest, persist_run_id set) stops at
#         MARK_BRANCH — mark_branch_step=1, a node_id is persisted
#   M9.4  POST /run/kvm/resume (fixed-tape) on that node reaches the guest's next MARK_BRANCH —
#         mark_branch_step=2, echoes the fresh suffix, persists a second node_id
#   M9.5  POST /run/kvm/branch (generate mode, mark-branch-guest, persist_run_id set) drives
#         baud-driver to generate branch tapes — every branch stops at MARK_BRANCH and persists
#   M9.6  POST /run/kvm/resume (generate mode) on the generate-mode branch point keeps exploring
#         with no kernel_path and no re-boot
#   M9.7  Error handling: branch_tapes_hex + generate together, invalid tape_hex, and resuming an
#         unknown run_id/node_id all return a JSON "error" field, never a panic/500

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-m9-XXXXXX.sqlite)"
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

log() { echo "[m9] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

HELLO_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/hello-guest/bzImage"
MARK_BRANCH_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/mark-branch-guest/bzImage"
TAPE_ECHO_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/tape-echo-guest/bzImage"
for k in "$HELLO_KERNEL" "$MARK_BRANCH_KERNEL" "$TAPE_ECHO_KERNEL"; do
    [[ -f "$k" ]] || fail "fixture kernel missing: $k"
done

# ---------------------------------------------------------------------------
# Build + start baud-server
# ---------------------------------------------------------------------------
log "Building baud-server..."
cargo build -q --bin baud-server 2>&1

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
# M9.1 — POST /run/kvm boots hello-guest twice, ram_hash identical
# ---------------------------------------------------------------------------
log "--- M9.1: POST /run/kvm — double boot, ram_hash identical ---"
RUN1=$(curl -sf -X POST "$SRV/run/kvm" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$HELLO_KERNEL\"}")
RUN2=$(curl -sf -X POST "$SRV/run/kvm" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$HELLO_KERNEL\"}")
OK1=$(echo "$RUN1" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$OK1" == "True" ]] || fail "M9.1: first /run/kvm returned ok!=true: $RUN1"
HASH1=$(echo "$RUN1" | python3 -c "import sys,json; print(json.load(sys.stdin)['ram_hash'])")
HASH2=$(echo "$RUN2" | python3 -c "import sys,json; print(json.load(sys.stdin)['ram_hash'])")
[[ -n "$HASH1" ]] || fail "M9.1: empty ram_hash"
[[ "$HASH1" == "$HASH2" ]] || fail "M9.1: ram_hash differs across two boots ($HASH1 vs $HASH2)"
pass "M9.1: POST /run/kvm — ram_hash identical across two boots ($HASH1)"

# ---------------------------------------------------------------------------
# M9.2 — POST /run/kvm/branch (fixed-tape, tape-echo-guest) — independent branches
# ---------------------------------------------------------------------------
log "--- M9.2: POST /run/kvm/branch — fixed-tape, tape-echo-guest, independent branches ---"
BRANCH_ECHO=$(curl -sf -X POST "$SRV/run/kvm/branch" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$TAPE_ECHO_KERNEL\", \"branch_tapes_hex\": [\"11aabbcc\", \"22aabbcc\", \"33aabbcc\"]}")
BRANCH_ECHO_OK=$(echo "$BRANCH_ECHO" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$BRANCH_ECHO_OK" == "True" ]] || fail "M9.2: /run/kvm/branch returned ok!=true: $BRANCH_ECHO"
echo "$BRANCH_ECHO" | python3 -c "
import sys, json
d = json.load(sys.stdin)
branches = d['branches']
assert len(branches) == 3, f'expected 3 branches, got {len(branches)}'
expected = ['11aabbcc', '22aabbcc', '33aabbcc']
for b, exp in zip(branches, expected):
    assert b['console_output_hex'] == exp, f\"branch echoed {b['console_output_hex']!r}, expected {exp!r} (cross-branch bleed)\"
    assert 'mark_branch_step' not in b, 'tape-echo-guest never calls MARK_BRANCH'
"
pass "M9.2: 3 branches, each echoed exactly its own suffix (no cross-branch bleed)"

# ---------------------------------------------------------------------------
# M9.3 — POST /run/kvm/branch (fixed-tape, mark-branch-guest, persist_run_id) — stops at MARK_BRANCH
# ---------------------------------------------------------------------------
log "--- M9.3: POST /run/kvm/branch — mark-branch-guest, persist_run_id set ---"
RUN_ID="m9-fixed-tape-$$"
BRANCH_MARK=$(curl -sf -X POST "$SRV/run/kvm/branch" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$MARK_BRANCH_KERNEL\", \"branch_tapes_hex\": [\"42\"], \"persist_run_id\": \"$RUN_ID\"}")
BRANCH_MARK_OK=$(echo "$BRANCH_MARK" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$BRANCH_MARK_OK" == "True" ]] || fail "M9.3: /run/kvm/branch returned ok!=true: $BRANCH_MARK"
MARK_STEP=$(echo "$BRANCH_MARK" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['mark_branch_step'])")
[[ "$MARK_STEP" == "1" ]] || fail "M9.3: expected mark_branch_step=1, got $MARK_STEP: $BRANCH_MARK"
CONSOLE_HEX=$(echo "$BRANCH_MARK" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['console_output_hex'])")
[[ "$CONSOLE_HEX" == "42" ]] || fail "M9.3: expected console_output_hex=42, got $CONSOLE_HEX"
NODE_ID=$(echo "$BRANCH_MARK" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['node_id'])")
[[ -n "$NODE_ID" && "$NODE_ID" != "None" ]] || fail "M9.3: expected a node_id on the MARK_BRANCH branch: $BRANCH_MARK"
PERSISTED_RUN_ID=$(echo "$BRANCH_MARK" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['run_id'])")
PERSISTED_NODE_ID=$(echo "$BRANCH_MARK" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['node_id'])")
[[ "$PERSISTED_RUN_ID" == "$RUN_ID" ]] || fail "M9.3: persisted.run_id mismatch: $PERSISTED_RUN_ID != $RUN_ID"
pass "M9.3: stopped at MARK_BRANCH (step=1), echoed 0x42, persisted node_id=$NODE_ID under run_id=$RUN_ID"

# ---------------------------------------------------------------------------
# M9.4 — POST /run/kvm/resume (fixed-tape) — reaches the guest's next MARK_BRANCH
# ---------------------------------------------------------------------------
log "--- M9.4: POST /run/kvm/resume — fixed-tape, no kernel_path, no re-boot ---"
RESUME1=$(curl -sf -X POST "$SRV/run/kvm/resume" -H "Content-Type: application/json" \
    -d "{\"run_id\": \"$RUN_ID\", \"node_id\": \"$NODE_ID\", \"branch_tapes_hex\": [\"42aa\"]}")
RESUME1_OK=$(echo "$RESUME1" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RESUME1_OK" == "True" ]] || fail "M9.4: /run/kvm/resume returned ok!=true: $RESUME1"
RESUME1_STEP=$(echo "$RESUME1" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['mark_branch_step'])")
[[ "$RESUME1_STEP" == "2" ]] || fail "M9.4: expected mark_branch_step=2, got $RESUME1_STEP: $RESUME1"
RESUME1_CONSOLE=$(echo "$RESUME1" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['console_output_hex'])")
[[ "$RESUME1_CONSOLE" == "42aa" ]] || fail "M9.4: expected console_output_hex=42aa (fresh suffix echoed), got $RESUME1_CONSOLE"
RESUME1_NODE_ID=$(echo "$RESUME1" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['node_id'])")
[[ -n "$RESUME1_NODE_ID" && "$RESUME1_NODE_ID" != "None" && "$RESUME1_NODE_ID" != "$NODE_ID" ]] \
    || fail "M9.4: expected a fresh, distinct node_id at the second MARK_BRANCH: $RESUME1"
pass "M9.4: resumed with no kernel_path/re-boot, reached second MARK_BRANCH (step=2), echoed 0x42 0xaa, new node_id=$RESUME1_NODE_ID"

# ---------------------------------------------------------------------------
# M9.5 — POST /run/kvm/branch (generate mode, mark-branch-guest, persist_run_id)
# ---------------------------------------------------------------------------
log "--- M9.5: POST /run/kvm/branch — generate mode, mark-branch-guest ---"
GEN_RUN_ID="m9-generate-$$"
BRANCH_GEN=$(curl -sf -X POST "$SRV/run/kvm/branch" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$MARK_BRANCH_KERNEL\", \"persist_run_id\": \"$GEN_RUN_ID\", \"generate\": {\"seed\": 7, \"count\": 3, \"tape_len_bytes\": 1}}")
BRANCH_GEN_OK=$(echo "$BRANCH_GEN" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$BRANCH_GEN_OK" == "True" ]] || fail "M9.5: /run/kvm/branch (generate) returned ok!=true: $BRANCH_GEN"
echo "$BRANCH_GEN" | python3 -c "
import sys, json
d = json.load(sys.stdin)
branches = d['branches']
assert len(branches) == 3, f'expected 3 generated branches, got {len(branches)}'
for b in branches:
    assert b['mark_branch_step'] == 1, f'expected mark_branch_step=1, got {b.get(\"mark_branch_step\")}'
    assert b.get('interesting') is True, 'a MARK_BRANCH stop must be reported interesting'
    assert b.get('node_id'), 'a MARK_BRANCH stop with persist_run_id set must persist a node_id'
assert d['driver_summary']['generations'] == 3
assert d['driver_summary']['cumulative_generation'] == 3, f\"expected cumulative_generation=3 on the first generate call, got {d['driver_summary']['cumulative_generation']}\"
assert d['persisted']['run_id'] == '$GEN_RUN_ID'
"
GEN_ROOT_NODE_ID=$(echo "$BRANCH_GEN" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['node_id'])")
GEN_BRANCH_NODE_ID=$(echo "$BRANCH_GEN" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['node_id'])")
GEN_BRANCH_TAPE_HEX=$(echo "$BRANCH_GEN" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['tape_hex'])")
pass "M9.5: generated 3 branches, all stopped at MARK_BRANCH and persisted under run_id=$GEN_RUN_ID"

# ---------------------------------------------------------------------------
# M9.6 — POST /run/kvm/resume (generate mode) — keeps exploring, no kernel_path
# ---------------------------------------------------------------------------
log "--- M9.6: POST /run/kvm/resume — generate mode, no kernel_path/re-boot ---"
# mark-branch-guest's restored tape cursor is already past the checkpoint byte, so (like the
# fixed-tape resume in M9.4) the *first* generated byte is never re-read — tape_len_bytes: 2 gives
# the guest's second loop iteration a real second byte to consume before its next MARK_BRANCH.
RESUME_GEN=$(curl -sf -X POST "$SRV/run/kvm/resume" -H "Content-Type: application/json" \
    -d "{\"run_id\": \"$GEN_RUN_ID\", \"node_id\": \"$GEN_BRANCH_NODE_ID\", \"generate\": {\"seed\": 13, \"count\": 2, \"tape_len_bytes\": 2}}")
RESUME_GEN_OK=$(echo "$RESUME_GEN" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RESUME_GEN_OK" == "True" ]] || fail "M9.6: /run/kvm/resume (generate) returned ok!=true: $RESUME_GEN"
echo "$RESUME_GEN" | python3 -c "
import sys, json
d = json.load(sys.stdin)
branches = d['branches']
assert len(branches) == 2, f'expected 2 generated branches, got {len(branches)}'
for b in branches:
    assert b['mark_branch_step'] == 2, f'expected mark_branch_step=2 (guests second checkpoint), got {b.get(\"mark_branch_step\")}'
    assert b.get('node_id'), 'resume (generate mode) must persist a node_id for every MARK_BRANCH stop, same as branch (generate mode)'
assert d['driver_summary']['cumulative_generation'] == 5, f\"expected cumulative_generation=5 (3 from M9.5 + 2 here), got {d['driver_summary']['cumulative_generation']} — Driver state did not resume across requests\"
"
pass "M9.6: resumed generate-mode branch point with no kernel_path, reached the guest's second MARK_BRANCH, persisted node_id per branch"

# ---------------------------------------------------------------------------
# M9.6b — a further resume (generate mode) call must keep accumulating the same Driver's
# generation counter, not just carry over once by accident — closes todo.md §14's "Driver state
# persistence across requests" gap end to end over real HTTP.
# ---------------------------------------------------------------------------
log "--- M9.6b: POST /run/kvm/resume — generate mode again, driver state keeps accumulating ---"
RESUME_GEN2=$(curl -sf -X POST "$SRV/run/kvm/resume" -H "Content-Type: application/json" \
    -d "{\"run_id\": \"$GEN_RUN_ID\", \"node_id\": \"$GEN_BRANCH_NODE_ID\", \"generate\": {\"seed\": 21, \"count\": 1, \"tape_len_bytes\": 2}}")
RESUME_GEN2_OK=$(echo "$RESUME_GEN2" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RESUME_GEN2_OK" == "True" ]] || fail "M9.6b: /run/kvm/resume (generate) returned ok!=true: $RESUME_GEN2"
echo "$RESUME_GEN2" | python3 -c "
import sys, json
d = json.load(sys.stdin)
assert d['driver_summary']['cumulative_generation'] == 6, f\"expected cumulative_generation=6 (5 + 1 more), got {d['driver_summary']['cumulative_generation']}\"
"
pass "M9.6b: a third resume (generate mode) call continued accumulating driver state (cumulative_generation=6)"

# ---------------------------------------------------------------------------
# M9.7 — Error handling: mutually-exclusive fields, invalid hex, unknown run/node
# ---------------------------------------------------------------------------
log "--- M9.7: error handling ---"

ERR_BOTH=$(curl -sf -X POST "$SRV/run/kvm/branch" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$HELLO_KERNEL\", \"branch_tapes_hex\": [\"aa\"], \"generate\": {\"seed\": 1, \"count\": 1}}")
ERR_BOTH_MSG=$(echo "$ERR_BOTH" | python3 -c "import sys,json; print(json.load(sys.stdin).get('error',''))")
[[ -n "$ERR_BOTH_MSG" ]] || fail "M9.7: expected an error for branch_tapes_hex+generate together: $ERR_BOTH"
pass "M9.7a: branch_tapes_hex + generate together → error ($ERR_BOTH_MSG)"

ERR_HEX=$(curl -sf -X POST "$SRV/run/kvm" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$HELLO_KERNEL\", \"tape_hex\": \"not-hex\"}")
ERR_HEX_MSG=$(echo "$ERR_HEX" | python3 -c "import sys,json; print(json.load(sys.stdin).get('error',''))")
[[ -n "$ERR_HEX_MSG" ]] || fail "M9.7: expected an error for invalid tape_hex: $ERR_HEX"
pass "M9.7b: invalid tape_hex → error ($ERR_HEX_MSG)"

ERR_UNKNOWN=$(curl -sf -X POST "$SRV/run/kvm/resume" -H "Content-Type: application/json" \
    -d "{\"run_id\": \"no-such-run-$$\", \"node_id\": \"$(printf '0%.0s' {1..64})\", \"branch_tapes_hex\": [\"aa\"]}")
ERR_UNKNOWN_MSG=$(echo "$ERR_UNKNOWN" | python3 -c "import sys,json; print(json.load(sys.stdin).get('error',''))")
[[ -n "$ERR_UNKNOWN_MSG" ]] || fail "M9.7: expected an error resuming an unknown run/node: $ERR_UNKNOWN"
pass "M9.7c: resuming an unknown run_id/node_id → error ($ERR_UNKNOWN_MSG)"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "==========================================="
echo "ALL M9 CHECKS PASSED"
echo "==========================================="
echo ""
echo "POST /run/kvm, /run/kvm/branch, /run/kvm/resume exercised end-to-end over real HTTP"
echo "against a real baud-server process on real /dev/kvm:"
echo "  /run/kvm            — boot to first halt, deterministic ram_hash across two boots"
echo "  /run/kvm/branch      — fixed-tape independent branches; MARK_BRANCH stop + persist"
echo "  /run/kvm/branch      — driver-generated branches; MARK_BRANCH stop + persist"
echo "  /run/kvm/resume      — fixed-tape and generate-mode resume, no kernel_path, no re-boot"
echo "  error handling       — mutually-exclusive fields, invalid hex, unknown run/node"
