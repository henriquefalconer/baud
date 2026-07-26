#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg-virtio-rng-generate-cli.sh — closes todo.md §14 next-actions item 1's last
# remaining virtio-rng gap: `--generate-seed`/`--generate-count` (baud-driver-generated
# branches) previously ran with virtio_rng always disabled, even when a caller set
# `--virtio-rng-seed` — silently ignored, not rejected. Two real bugs are fixed together
# here: the server route (`run_driver_generated_branches_with_persist` never threaded
# `virtio_rng` to the branches it forks) and the CLI itself (`baud kvm-branch`/
# `kvm-resume` only ever put `virtio_rng` in the JSON body inside the fixed-tape
# `else` arm, so `--virtio-rng-seed` silently vanished whenever `--generate-seed` was
# also set, before the request even left the client).
#
#   RNG-GEN.1 `baud run kvm-branch --generate-seed --generate-count --virtio-rng-seed` —
#       virtio-rng-guest, every generated branch's own ISR observes the real seeded
#       entropy byte (virtio-rng-guest ignores its tape suffix entirely, so this proves
#       the interrupt was actually delivered, not that the tape happened to match).
#   RNG-GEN.2 `baud run kvm-resume --generate-seed --generate-count --virtio-rng-seed` —
#       resuming that persisted branch point, forking again in generate mode with
#       virtio_rng, reproduces the identical ISR output with no re-boot.

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
BAUD="$REPO_ROOT/target/debug/baud"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-pkg-rng-gen-XXXXXX.sqlite)"
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

log() { echo "[pkg-rng-gen] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

VIRTIO_RNG_GUEST_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/virtio-rng-guest/bzImage"
[[ -f "$VIRTIO_RNG_GUEST_KERNEL" ]] || fail "fixture kernel missing: $VIRTIO_RNG_GUEST_KERNEL"

log "Building baud-server and baud CLI..."
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

# ---------------------------------------------------------------------------
# RNG-GEN.1 — baud run kvm-branch --generate-seed/--generate-count --virtio-rng-seed
# ---------------------------------------------------------------------------
log "--- RNG-GEN.1: kvm-branch --generate-seed --virtio-rng-seed — virtio-rng-guest ---"
STORE_RUN_ID="pkg-rng-gen-store-$$"
BRANCH_JSON="$("$BAUD" run kvm-branch \
    --kernel "$VIRTIO_RNG_GUEST_KERNEL" \
    --persist-run-id "$STORE_RUN_ID" \
    --generate-seed 7 \
    --generate-count 3 \
    --generate-tape-len-bytes 4 \
    --virtio-rng-seed 42 \
    --virtio-rng-vector 49 \
    --virtio-rng-max-exits 200000 \
    --json)" || fail "kvm-branch generate mode FAILED to run"
BRANCH_OK=$(echo "$BRANCH_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$BRANCH_OK" == "True" ]] || fail "RNG-GEN.1: kvm-branch returned ok!=true: $BRANCH_JSON"

CONSOLES=$(echo "$BRANCH_JSON" | python3 -c "
import sys, json
d = json.load(sys.stdin)
for b in d['branches']:
    print(b['console_output_hex'])
")
[[ "$(echo "$CONSOLES" | wc -l)" == "3" ]] || fail "RNG-GEN.1: expected 3 generated branches, got: $CONSOLES"
while IFS= read -r console; do
    [[ "$console" == "5295" ]] || fail "RNG-GEN.1: unexpected generated-branch console output: $console (want 5295 — the ISR marker + seed-42 entropy byte, same regardless of the generated tape suffix since virtio-rng-guest never reads its tape at all)"
done <<< "$CONSOLES"
NODE_ID=$(echo "$BRANCH_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['node_id'])")
pass "RNG-GEN.1: all 3 driver-generated branches delivered the real interrupt (console=5295 each), persisted node_id=$NODE_ID"

# ---------------------------------------------------------------------------
# RNG-GEN.2 — baud run kvm-resume --generate-seed/--generate-count --virtio-rng-seed
# ---------------------------------------------------------------------------
log "--- RNG-GEN.2: kvm-resume --generate-seed --virtio-rng-seed — no re-boot ---"
RESUME_JSON="$("$BAUD" run kvm-resume \
    --run-id "$STORE_RUN_ID" \
    --node-id "$NODE_ID" \
    --generate-seed 11 \
    --generate-count 2 \
    --generate-tape-len-bytes 4 \
    --virtio-rng-seed 42 \
    --virtio-rng-vector 49 \
    --virtio-rng-max-exits 200000 \
    --json)" || fail "kvm-resume generate mode FAILED to run"
RESUME_OK=$(echo "$RESUME_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RESUME_OK" == "True" ]] || fail "RNG-GEN.2: kvm-resume returned ok!=true: $RESUME_JSON"

RESUME_CONSOLES=$(echo "$RESUME_JSON" | python3 -c "
import sys, json
d = json.load(sys.stdin)
for b in d['branches']:
    print(b['console_output_hex'])
")
[[ "$(echo "$RESUME_CONSOLES" | wc -l)" == "2" ]] || fail "RNG-GEN.2: expected 2 generated branches, got: $RESUME_CONSOLES"
while IFS= read -r console; do
    [[ "$console" == "5295" ]] || fail "RNG-GEN.2: unexpected resumed generated-branch console output: $console"
done <<< "$RESUME_CONSOLES"
pass "RNG-GEN.2: resuming with generate mode + virtio_rng reproduced the identical ISR output with no re-boot"

curl -sf http://127.0.0.1:7734/health > /dev/null || fail "baud-server is no longer healthy at the end of the run"
pass "baud-server remained healthy throughout"

echo ""
echo "==========================================="
echo "ALL PKG-VIRTIO-RNG-GENERATE CHECKS PASSED"
echo "==========================================="
echo ""
echo "/run/kvm/branch and /run/kvm/resume's generate (baud-driver) mode now honors virtio_rng"
echo "exactly like the fixed-tape mode, exercised end-to-end through the real 'baud' CLI"
echo "against a real baud-server process on real /dev/kvm:"
echo "  baud run kvm-branch --generate-seed --virtio-rng-seed  — every generated branch delivers the interrupt"
echo "  baud run kvm-resume --generate-seed --virtio-rng-seed  — a resumed generate call reproduces it, no re-boot"
echo ""
echo "This also fixes a real CLI-side bug found while building this: kvm-branch/kvm-resume"
echo "only ever placed --virtio-rng-seed into the request body inside the fixed-tape branch"
echo "of the match, so it silently vanished whenever --generate-seed was set, before the"
echo "request even reached the server."
echo ""
echo "Still open (todo.md §14): the deeper 'which vector an unmodified Linux guest's own"
echo "virtio_mmio driver would bind to' research question remains untouched; no fixture yet"
echo "both drives virtio-rng and emits frames in the same guest."
