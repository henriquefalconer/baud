#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h/h9.sh — H9 drive script (partial): `baud verify fingerprint`, real CLI/server end-to-end
#
# H9's full spec is a full unmodified distro, cross-VM determinism: boot the stock Ubuntu 18.04.1
# LTS image on two independent VMs and compare a timed-exit fingerprint (todo.md §10/§14 item 9).
# That still needs the real Ubuntu cloud image (H9 (d)/(e), unstarted) and the true two-separate-
# process/two-core orchestration (`baud_multiverse::linux::run_fleet` is the closest existing
# primitive, still per-thread not per-process, and not wired to any route).
#
# This script demonstrates the piece that WAS missing before this iteration: `baud verify
# fingerprint` through a real CLI invocation against a live `baud-server` over real HTTP
# (POST /verify/fingerprint, crates/baud-server/src/routes/verify_fingerprint.rs), reusing the
# already-hardware-tested `baud-fingerprint` crate (todo.md §14 item 9) and the already-built
# timer-guest fixture — the same same-process-sequential stand-in
# `baud_fingerprint::linux::tests::two_independent_boots_produce_matching_fingerprints` uses for
# H9's true cross-process orchestration.
#
#   H9.1  baud host probe still reports runnable=true (cheap, early sanity check)
#   H9.2  `baud verify fingerprint --times 2` on timer-guest: two independent boots produce
#         matching fingerprints (ok=true, no divergence) through the real CLI/server path
#   H9.3  `baud verify fingerprint --expected-banner <banner timer-guest never prints>`: the route
#         refuses to report a fingerprint for the wrong point (exit 1, not a false pass)

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[h9] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H9 (partial): baud verify fingerprint, real CLI/server end-to-end ==="
echo ""

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
BAUD="$REPO_ROOT/target/debug/baud"
KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/timer-guest/bzImage"
DB_FILE="$(mktemp -u -t baud-h9-XXXXXX.sqlite)"
SNAP_ROOT="$(mktemp -d -t baud-h9-snap-XXXXXX)"
SERVER_PID=""

[[ -f "$KERNEL" ]] || fail "fixture missing: $KERNEL"

# Own port + own snapshot store, so this script can run concurrently with any other drive script.
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
# interrupted -- Ctrl-C, or drive/gate.sh reaping its pool.
trap 'exit 130' INT
trap 'exit 143' TERM

log "Building baud-host/baud-server/baud-cli..."
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q -p baud-host -p baud-server -p baud-cli 2>&1
fi

log "Starting baud-server on $SRV..."
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$BAUD_PORT" \
    BAUD_SNAPSHOT_STORE="$SNAP_ROOT" BAUD_LOG=warn "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for _ in $(seq 1 60); do
    if curl -sf "$SRV/health" > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf "$SRV/health" > /dev/null 2>&1 || fail "baud-server did not come up on $SRV"

# ---------------------------------------------------------------------------
# H9.1 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# ---------------------------------------------------------------------------
log "baud host probe --json"
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H9.1: 'baud host probe --json' FAILED to run"
RUNNABLE="$(echo "$PROBE_JSON" | grep -oE '"runnable":[[:space:]]*(true|false)' | grep -oE 'true|false')"
if [[ "$RUNNABLE" != "true" ]]; then
    fail "H9.1: host probe runnable is '$RUNNABLE' — no real /dev/kvm, H9 cannot mean anything here."
fi
pass "H9.1: host probe runnable='$RUNNABLE' (real KVM present)"

# ---------------------------------------------------------------------------
# H9.2 — two independent boots must produce matching fingerprints
# ---------------------------------------------------------------------------
log "baud verify fingerprint --kernel $KERNEL --target-rcb 100000 --times 2 ..."
FP_JSON="$("$BAUD" verify fingerprint \
    --kernel "$KERNEL" \
    --cmdline "console=ttyS0" \
    --target-rcb 100000 \
    --times 2 \
    --json)" || fail "H9.2: 'baud verify fingerprint' FAILED to run"
echo "$FP_JSON"

OK="$(echo "$FP_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")"
[[ "$OK" == "True" ]] || fail "H9.2: 'baud verify fingerprint' reported ok!=true: $FP_JSON"
DIVERGENCE="$(echo "$FP_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('divergence'))")"
[[ "$DIVERGENCE" == "None" ]] || fail "H9.2: unexpected divergence reported: $DIVERGENCE"
pass "H9.2: two independent boots of timer-guest produced matching fingerprints through the real CLI/server path"

# ---------------------------------------------------------------------------
# H9.3 — a banner the guest never prints must fail the capture, not silently pass
# ---------------------------------------------------------------------------
log "baud verify fingerprint --expected-banner '<never printed>' (must fail)..."
set +e
BAD_FP_JSON="$("$BAUD" verify fingerprint \
    --kernel "$KERNEL" \
    --cmdline "console=ttyS0" \
    --target-rcb 100000 \
    --expected-banner "a banner timer-guest never prints" \
    --times 2 \
    --json)"
BAD_STATUS=$?
set -e
echo "$BAD_FP_JSON"
[[ "$BAD_STATUS" -ne 0 ]] || fail "H9.3: 'baud verify fingerprint' with an unreachable banner exited 0"
echo "$BAD_FP_JSON" | grep -q "NoBanner\|did not reach the expected banner" \
    || fail "H9.3: failure did not name the real cause: $BAD_FP_JSON"
pass "H9.3: an unreachable expected banner fails the capture (exit $BAD_STATUS), not a silent false pass"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H9 (partial) milestone: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (runnable=$RUNNABLE):"
echo "  - POST /verify/fingerprint (crates/baud-server/src/routes/verify_fingerprint.rs) boots a"
echo "    guest N times, captures a timed-exit fingerprint from each (baud-fingerprint's capture()),"
echo "    and compares them, closing todo.md §14 item 9's 'baud verify fingerprint CLI/HTTP route'"
echo "    gap"
echo "  - 'baud verify fingerprint' (crates/baud-cli/src/cmds/verify.rs) drives it end-to-end over"
echo "    real HTTP, exit code 0 on a match and 1 on a divergence or capture error"
echo "  - A banner the guest never reaches fails the whole call loud (FpError::NoBanner), never a"
echo "    fingerprint for the wrong point"
echo ""
echo "Still open for full H9 (todo.md §14): the true two-separate-process/two-core orchestration"
echo "(this script's two boots are still same-process-sequential, like baud-fingerprint's own"
echo "cross-VM test) and the real Ubuntu 18.04.1 cloud-image acquisition/boot (H9 (d)/(e))."
