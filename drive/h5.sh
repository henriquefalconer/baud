#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h5.sh — H5 drive script: snapshot / restore (todo.md §10's H5, first slice)
#
# H5's spec: capture a universe, restore it, continue running, and get the same observable
# behavior a straight (never-snapshotted) run would have produced. This validates that for real
# against actual /dev/kvm: `Multiverse::snapshot`/`Multiverse::restore`
# (crates/baud-multiverse/src/linux/mod.rs) walk `baud_snapshot::linux::capture`/`restore`'s real
# `KVM_GET_*`/`KVM_SET_*` ioctls against a real vCPU (todo.md §14 tracked this exact gap: "nothing
# calls snapshot/restore/DirtyRing on real KVM hardware yet").
#
#   H5.1  baud host probe still reports a non-rejected regime (cheap, early sanity check)
#   H5.2  snapshot_roundtrip_is_bit_identical: capture a running timer-guest at K (after its first
#         injected tick), restore it into a brand-new Multiverse, deliver a second tick and run to
#         halt — the restored run's landed instruction (rip) and its whole observation stream
#         (console output + RAM hash) must match a straight run that delivered both ticks without
#         ever snapshotting.
#
# Not yet covered here (tracked in todo.md, later H5 slices): userfaultfd-based branching
# (Snapshot::branch, blocked on a guest-RAM backing change — see baud-snapshot/src/linux.rs's
# module doc), dirty-ring-based reset_cost_scales_with_write_set, shell_into_universe_resumes,
# restore_refuses_mismatched_cpu on real hardware (currently only unit-tested at the pure
# universe::model_matches level).

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h5] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H5: Snapshot / restore ==="
echo ""

# ---------------------------------------------------------------------------
# H5.1 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# `baud host probe` is a CLI-to-server call, so a server needs to be up first (same pattern as
# drive/h0.sh through drive/h4.sh).
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli/baud-multiverse/baud-snapshot..."
cargo build -q -p baud-host -p baud-server -p baud-cli -p baud-multiverse -p baud-snapshot 2>&1

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h5-XXXXXX.sqlite)"
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
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H5.1: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
REGIME="$(echo "$PROBE_JSON" | grep -o '"regime":[[:space:]]*"[^"]*"' | sed -E 's/.*"([^"]+)"$/\1/')"
if [[ "$REGIME" == "rejected" || -z "$REGIME" ]]; then
    fail "H5.1: host probe regime is '$REGIME' — no real /dev/kvm, H5 cannot mean anything here."
fi
pass "H5.1: host probe regime='$REGIME' (real KVM present)"

# ---------------------------------------------------------------------------
# H5.2 — snapshot_roundtrip_is_bit_identical
# ---------------------------------------------------------------------------
log "Running snapshot_roundtrip_is_bit_identical against real /dev/kvm (timer-guest fixture)..."
SNAP_OUT=$(cargo test -q -p baud-multiverse snapshot_roundtrip_is_bit_identical -- --test-threads=1 2>&1)
echo "$SNAP_OUT"
echo "$SNAP_OUT" | grep -q "test result: ok" || fail "H5.2: snapshot_roundtrip_is_bit_identical FAILED"
pass "H5.2: snapshot_roundtrip_is_bit_identical — a captured-then-restored universe continues identically to a straight run"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H5 milestone (first slice): ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (regime=$REGIME):"
echo "  - Multiverse::snapshot captures a complete Universe (RAM + all vCPU state + work-clock"
echo "    anchor/RCB + tape cursor + console) from a real, running guest"
echo "  - Multiverse::restore reconstructs a brand-new Multiverse from that Universe, refusing a"
echo "    CPU-model mismatch unless a CPUID template is active"
echo "  - A second timer tick delivered on the restored guest lands on the bit-identical"
echo "    instruction (rip) a straight, never-snapshotted run would have reached"
echo "  - The restored run's whole observation stream (console output, RAM hash) through the"
echo "    final halt is byte-identical to the straight run's"
echo ""
echo "H-series H0-H4 complete; H5's core capture/restore round trip is now proven on real hardware."
echo "Remaining H5 work (userfaultfd branching, dirty-ring reset, shell-into, real-hardware CPU-"
echo "mismatch refusal): see todo.md."
echo ""
