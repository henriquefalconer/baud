#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h/h4.sh — H4 drive script: interrupt at an exact instruction boundary (todo.md §10's H4)
#
# H4's spec: "Deliver a timer tick (or any interrupt) at a chosen work-count via
# arm-early-then-single-step; identical instruction across a double-run." This validates that for
# real against actual /dev/kvm: `Multiverse::inject_timer_tick`/`run_with_timer_ticks`
# (crates/baud-multiverse/src/linux/mod.rs) drive `baud_vcpu::boundary::inject_at` against a real
# vCPU and a real IDT-registered handler (tests/fixtures/timer-guest/, see its own BUILD.md), not
# just the hardware-independent scripted-stepper tests `baud-vcpu`'s own unit tests already cover.
#
#   H4.1  baud host probe still reports runnable=true (cheap, early sanity check)
#   H4.2  timer_tick_lands_at_identical_instruction: two ticks injected at chosen work-counts land
#         on the bit-identical instruction (rip) across two boots of the same image+tape, the
#         guest actually takes each interrupt exactly once in order, and the final halt state
#         (console output, RAM hash) is itself identical across both boots.

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h4] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H4: Interrupt at an exact instruction boundary ==="
echo ""

# ---------------------------------------------------------------------------
# H4.1 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# `baud host probe` is a CLI-to-server call, so a server needs to be up first (same pattern as
# drive/h/h0.sh through drive/h/h3.sh).
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli/baud-multiverse..."
# BAUD_GATE_PREBUILT: set by a gate that has already built the workspace, so the (~7s, target-dir
# locking) no-op `cargo build` below can be skipped when many drive scripts run concurrently.
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q -p baud-host -p baud-server -p baud-cli -p baud-multiverse 2>&1
fi

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h4-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"

# Own port + own snapshot store, so this script can run concurrently with any other drive script.
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"
SNAP_ROOT="$(mktemp -d -t baud-h4-snap-XXXXXX)"

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

log "Starting baud-server on $SRV..."
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$BAUD_PORT" \
    BAUD_SNAPSHOT_STORE="$SNAP_ROOT" "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for _ in $(seq 1 60); do
    if curl -sf "$SRV/health" > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf "$SRV/health" > /dev/null 2>&1 || fail "baud-server did not come up on $SRV"

log "baud host probe --json"
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H4.1: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
RUNNABLE="$(echo "$PROBE_JSON" | grep -oE '"runnable":[[:space:]]*(true|false)' | grep -oE 'true|false')"
if [[ "$RUNNABLE" != "true" ]]; then
    fail "H4.1: host probe runnable is '$RUNNABLE' — no real /dev/kvm, H4 cannot mean anything here."
fi
pass "H4.1: host probe runnable='$RUNNABLE' (real KVM present)"

# ---------------------------------------------------------------------------
# H4.2 — timer_tick_lands_at_identical_instruction
# ---------------------------------------------------------------------------
log "Running timer_tick_lands_at_identical_instruction against real /dev/kvm (timer-guest fixture)..."
TIMER_OUT=$(cargo test -q -p baud-multiverse timer_tick_lands_at_identical_instruction -- --test-threads=1 2>&1)
echo "$TIMER_OUT"
echo "$TIMER_OUT" | grep -q "test result: ok" || fail "H4.2: timer_tick_lands_at_identical_instruction FAILED"
pass "H4.2: timer_tick_lands_at_identical_instruction — two injected ticks land on the bit-identical instruction across two boots"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H4 milestone: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (runnable=$RUNNABLE):"
echo "  - Multiverse::inject_timer_tick drives the real arm-early-then-single-step engine"
echo "    (baud_vcpu::boundary::inject_at) against a real vCPU and a real IDT-registered handler"
echo "  - Two ticks injected at chosen work-counts land on the bit-identical instruction (rip)"
echo "    across two boots of the same image+tape"
echo "  - The guest actually takes each injected interrupt exactly once, in order"
echo "  - Final halt state (console output, RAM hash) is identical across both boots even with"
echo "    interrupts injected mid-run"
echo ""
echo "H-series H0-H4 complete. Proceed to H5: ./drive/h/h5.sh"
