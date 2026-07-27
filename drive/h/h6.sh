#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h/h6.sh — H6 drive script: multi-VM fleet (todo.md §10's H6)
#
# H6's spec: many single-vCPU VMs pinned across physical cores explore in parallel on one host.
# This validates that for real against actual /dev/kvm via
# crates/baud-multiverse/src/linux/mod.rs's `run_fleet` and its named test
# `fleet_of_vms_run_in_parallel_without_interference`:
#
#   H6.1  baud host probe reports runnable=true (cheap, early sanity check).
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

cd "$(dirname "$0")/../.."

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
# drive/h/h0.sh through drive/h/h5.sh).
# ---------------------------------------------------------------------------
log "Building baud-host/baud-server/baud-cli/baud-multiverse/baud-vcpu..."
# BAUD_GATE_PREBUILT: set by a gate that has already built the workspace, so the (~7s, target-dir
# locking) no-op `cargo build` below can be skipped when many drive scripts run concurrently.
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q -p baud-host -p baud-server -p baud-cli -p baud-multiverse -p baud-vcpu 2>&1
fi

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h6-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"

# Own port + own snapshot store, so this script's *server* can coexist with any other drive
# script's. NOTE: H6.2 itself still needs a quiet machine — it pins threads to fixed cores and
# asserts real speedup over a measured serial baseline, so it fails legitimately under concurrent
# load (and two concurrent h6.sh runs collide on the same cores).
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"
SNAP_ROOT="$(mktemp -d -t baud-h6-snap-XXXXXX)"

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
PROBE_JSON="$("$BAUD" host probe --json)" || fail "H6.1: 'baud host probe --json' FAILED to run"
echo "$PROBE_JSON"
RUNNABLE="$(echo "$PROBE_JSON" | grep -oE '"runnable":[[:space:]]*(true|false)' | grep -oE 'true|false')"
if [[ "$RUNNABLE" != "true" ]]; then
    fail "H6.1: host probe runnable is '$RUNNABLE' — no real /dev/kvm, H6 cannot mean anything here."
fi
pass "H6.1: host probe runnable='$RUNNABLE' (real KVM present)"

# ---------------------------------------------------------------------------
# H6.2 — fleet_of_vms_run_in_parallel_without_interference
# ---------------------------------------------------------------------------
log "Running fleet_of_vms_run_in_parallel_without_interference against real /dev/kvm (tape-echo-guest fixture, N real concurrent VM lifecycles)..."
# `--include-ignored` (not `--ignored`): this test is #[ignore]d in-tree because it pins threads to
# fixed cores and asserts a timing ratio against a serial baseline, so it only means anything on a
# quiet machine — here — and not inside a `cargo test --workspace` running it next to 7 sibling KVM
# tests. `--include-ignored` runs it in BOTH states (ignored or not), whereas `--ignored` would
# silently run *nothing* if the attribute is ever removed. The "[1-9] passed" assertion below makes
# a zero-tests-ran result a hard failure rather than a false pass, since "test result: ok. 0 passed"
# also matches "ok".
FLEET_OUT=$(cargo test -q -p baud-multiverse fleet_of_vms_run_in_parallel_without_interference -- --include-ignored --nocapture --test-threads=1 2>&1)
echo "$FLEET_OUT"
echo "$FLEET_OUT" | grep -q "test result: ok" || fail "H6.2: fleet_of_vms_run_in_parallel_without_interference FAILED"
echo "$FLEET_OUT" | grep -qE "test result: ok\. [1-9][0-9]* passed" \
    || fail "H6.2: fleet_of_vms_run_in_parallel_without_interference ran 0 tests (filtered out, never executed) — a false pass"
pass "H6.2: fleet_of_vms_run_in_parallel_without_interference — N single-vCPU VMs placed one-per-core, pinned via sched_setaffinity, ran concurrently with no cross-VM interference and real speedup over a serial baseline"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H6 milestone: ALL CHECKS PASSED ==="
echo ""
echo "Demonstrated on real /dev/kvm (runnable=$RUNNABLE):"
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
