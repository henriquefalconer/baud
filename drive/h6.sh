#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h6.sh — H6 drive script: multi-VM fleet (todo.md §10's H6)
#
# H6's spec: many single-vCPU VMs pinned across physical cores explore in parallel on one host.
# This validates that for real against actual /dev/kvm via
# crates/baud-multiverse/src/linux/mod.rs's `run_fleet` and its named test
# `fleet_of_vms_run_in_parallel_without_interference`:
#
#   H6.1  baud host probe reports a non-rejected regime (cheap, early sanity check).
#   H6.2  fleet_of_vms_run_in_parallel_without_interference, which closes all three of H6's
#         milestone bullets in one real-hardware test:
#           - capacity_refuses_sibling_split against this host's *real* probed topology (not
#             baud-host's own fake-topology unit test): placing one VM over real capacity is
#             refused, and a full-capacity placement never splits an SMT sibling pair.
#           - no cross-VM interference: `run_fleet` places N real single-vCPU VMs one per
#             physical core (baud_host::Host::place), pins each VM's own OS thread to its core
#             (baud_vcpu::linux::pin_thread_to_core — this test is that function's first real
#             call site in the workspace, todo.md §14), boots tape-echo-guest on each with its own
#             unique 4-byte tape suffix, and asserts every VM's console output matches exactly its
#             own suffix — any VM observing another's state would surface as a mismatch.
#           - aggregate throughput: running the N-VM fleet concurrently is asserted to take well
#             under N times a measured single-VM serial baseline, proving the VMs actually run in
#             parallel rather than being silently serialized.
#
# Not yet covered here (tracked in todo.md): a multi-host fleet, and scheduling exploration
# (baud-driver) across the fleet's VMs — H6 as specified is one host's worth of parallel VMs.

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h6] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== H6: Multi-VM fleet ==="
echo ""

# ---------------------------------------------------------------------------
# H6.1 (checked first — cheap, and everything below is meaningless without it) — real KVM present.
# `baud host probe` is a CLI-to-server call, so a server needs to be up first (same pattern as
# drive/h0.sh through drive/h5.sh).
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli/baud-multiverse/baud-vcpu..."
cargo build -q -p baud-host -p baud-server -p baud-cli -p baud-multiverse -p baud-vcpu 2>&1

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h6-XXXXXX.sqlite)"
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
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H6.1: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
REGIME="$(echo "$PROBE_JSON" | grep -o '"regime":[[:space:]]*"[^"]*"' | sed -E 's/.*"([^"]+)"$/\1/')"
if [[ "$REGIME" == "rejected" || -z "$REGIME" ]]; then
    fail "H6.1: host probe regime is '$REGIME' — no real /dev/kvm, H6 cannot mean anything here."
fi
pass "H6.1: host probe regime='$REGIME' (real KVM present)"

# ---------------------------------------------------------------------------
# H6.2 — fleet_of_vms_run_in_parallel_without_interference
# ---------------------------------------------------------------------------
log "Running fleet_of_vms_run_in_parallel_without_interference against real /dev/kvm (tape-echo-guest fixture, N real concurrent VM lifecycles)..."
FLEET_OUT=$(cargo test -q -p baud-multiverse fleet_of_vms_run_in_parallel_without_interference -- --nocapture --test-threads=1 2>&1)
echo "$FLEET_OUT"
echo "$FLEET_OUT" | grep -q "test result: ok" || fail "H6.2: fleet_of_vms_run_in_parallel_without_interference FAILED"
pass "H6.2: fleet_of_vms_run_in_parallel_without_interference — N single-vCPU VMs placed one-per-core, pinned via sched_setaffinity, ran concurrently with no cross-VM interference and real speedup over a serial baseline"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H6 milestone: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (regime=$REGIME):"
echo "  - baud_host::Host::place refuses to place a fleet over this host's real capacity, and a"
echo "    full-capacity placement never splits an SMT sibling pair (capacity_refuses_sibling_split,"
echo "    exercised against the real probed topology, not a synthetic one)"
echo "  - baud_multiverse::linux::run_fleet places N real single-vCPU VMs one per physical core and"
echo "    pins each VM's own thread to its core via sched_setaffinity"
echo "    (baud_vcpu::linux::pin_thread_to_core, previously written but never called)"
echo "  - Every VM in the fleet echoes exactly its own tape suffix — no VM observes another's state"
echo "  - The N-VM fleet runs concurrently in well under N times a measured single-VM serial"
echo "    baseline, proving real parallel execution rather than silent serialization"
echo ""
echo "H0-H6 are now all demonstrated on real KVM hardware. Remaining work (the M-series: driver,"
echo "snapshot-store wiring, framebuffer stream, the baud shell-into CLI/server verb, the enforced-"
echo "regime custom KVM module): see todo.md."
echo ""
