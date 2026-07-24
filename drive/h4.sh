#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h4.sh — H4 drive script: interrupt at an exact instruction boundary (todo.md §10's H4)
#
# H4's spec: "Deliver a timer tick (or any interrupt) at a chosen work-count via
# arm-early-then-single-step; identical instruction across a double-run." This validates that for
# real against actual /dev/kvm: `Multiverse::inject_timer_tick`/`run_with_timer_ticks`
# (crates/baud-multiverse/src/linux/mod.rs) drive `baud_vcpu::boundary::inject_at` against a real
# vCPU and a real IDT-registered handler (tests/fixtures/timer-guest/, see its own BUILD.md), not
# just the hardware-independent scripted-stepper tests `baud-vcpu`'s own unit tests already cover.
#
#   H4.1  baud host probe still reports a non-rejected regime (cheap, early sanity check)
#   H4.2  timer_tick_lands_at_identical_instruction: two ticks injected at chosen work-counts land
#         on the bit-identical instruction (rip) across two boots of the same image+tape, the
#         guest actually takes each interrupt exactly once in order, and the final halt state
#         (console output, RAM hash) is itself identical across both boots.

set -euo pipefail

cd "$(dirname "$0")/.."

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
# drive/h0.sh through drive/h3.sh).
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli/baud-multiverse..."
cargo build -q -p baud-host -p baud-server -p baud-cli -p baud-multiverse 2>&1

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h4-XXXXXX.sqlite)"
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

log "Starting baud-server..."
pkill -f "baud-server" 2>/dev/null || true; sleep 0.2
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" "$BAUD_SERVER_BIN" &
SERVER_PID=$!
sleep 1

log "baud host probe --json"
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H4.1: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
REGIME="$(echo "$PROBE_JSON" | grep -o '"regime":[[:space:]]*"[^"]*"' | sed -E 's/.*"([^"]+)"$/\1/')"
if [[ "$REGIME" == "rejected" || -z "$REGIME" ]]; then
    fail "H4.1: host probe regime is '$REGIME' — no real /dev/kvm, H4 cannot mean anything here."
fi
pass "H4.1: host probe regime='$REGIME' (real KVM present)"

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
echo "Demonstrated on real /dev/kvm (regime=$REGIME):"
echo "  - Multiverse::inject_timer_tick drives the real arm-early-then-single-step engine"
echo "    (baud_vcpu::boundary::inject_at) against a real vCPU and a real IDT-registered handler"
echo "  - Two ticks injected at chosen work-counts land on the bit-identical instruction (rip)"
echo "    across two boots of the same image+tape"
echo "  - The guest actually takes each injected interrupt exactly once, in order"
echo "  - Final halt state (console output, RAM hash) is identical across both boots even with"
echo "    interrupts injected mid-run"
echo ""
echo "H-series H0-H4 complete. Proceed to H5: ./drive/h5.sh"
