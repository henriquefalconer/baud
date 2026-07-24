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
#   H5.3  restore_refuses_mismatched_cpu: a Universe with a forged (bit-flipped) cpu_signature is
#         refused by Multiverse::restore against this real host's real signature unless a CPUID
#         template is active — exercised against the real cpuid_leaf1_eax(kvm) read and the real
#         RestoreError::CpuMismatch path, not just the pure universe::model_matches comparator.
#   H5.4  reset_cost_scales_with_write_set: with a dirty ring negotiated at boot, running
#         timer-guest through two ticks dirties only a handful of RAM pages (the ISR's stack
#         pushes/pops plus a few page-table ACCESSED-bit updates), never total RAM; rewinding via
#         Multiverse::reset_dirty_pages makes guest RAM byte-identical to the pristine pre-run
#         snapshot again.
#   H5.5  thousand_branches_are_independent_and_deterministic: Multiverse::branch forks 1000+
#         independent continuations from one captured Universe (tape-echo-guest), each on its own
#         tape suffix — every branch's output matches exactly its own suffix (no cross-branch
#         perturbation), and a sampled subset is proven internally deterministic via a second
#         re-fork. Realized via the spec's documented "fork() copy-on-write is the small-N
#         fallback" (as Multiverse::restore, not a literal fork(2) — see Multiverse::branch's doc
#         for why a raw fork() can't safely share this process's KVM vm/vcpu fds), not the spec's
#         literal UFFDIO_CONTINUE memory-sharing mechanism, which remains blocked on a guest-RAM
#         backing change (see baud-snapshot/src/linux.rs's module doc).
#
# Not yet covered here (tracked in todo.md): true memory-efficient CoW branching (UFFDIO_CONTINUE
# over a shared memfd backing — the "cheap" half of Snapshot::branch's guarantee), and
# shell_into_universe_resumes.

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
# H5.3 — restore_refuses_mismatched_cpu
# ---------------------------------------------------------------------------
log "Running restore_refuses_mismatched_cpu against real /dev/kvm (hello-guest fixture)..."
CPU_OUT=$(cargo test -q -p baud-multiverse restore_refuses_mismatched_cpu -- --test-threads=1 2>&1)
echo "$CPU_OUT"
echo "$CPU_OUT" | grep -q "test result: ok" || fail "H5.3: restore_refuses_mismatched_cpu FAILED"
pass "H5.3: restore_refuses_mismatched_cpu — a forged cpu_signature is refused, and template_active=true bypasses the refusal"

# ---------------------------------------------------------------------------
# H5.4 — reset_cost_scales_with_write_set
# ---------------------------------------------------------------------------
log "Running reset_cost_scales_with_write_set against real /dev/kvm (timer-guest fixture)..."
DIRTY_OUT=$(cargo test -q -p baud-multiverse reset_cost_scales_with_write_set -- --test-threads=1 2>&1)
echo "$DIRTY_OUT"
echo "$DIRTY_OUT" | grep -q "test result: ok" || fail "H5.4: reset_cost_scales_with_write_set FAILED"
pass "H5.4: reset_cost_scales_with_write_set — dirty-ring reset touches a handful of pages, never total RAM, and rewinds RAM exactly"

# ---------------------------------------------------------------------------
# H5.5 — thousand_branches_are_independent_and_deterministic
# ---------------------------------------------------------------------------
log "Running thousand_branches_are_independent_and_deterministic against real /dev/kvm (tape-echo-guest fixture, ~1000 real VM lifecycles — this takes a few minutes)..."
BRANCH_OUT=$(cargo test -q -p baud-multiverse thousand_branches_are_independent_and_deterministic -- --test-threads=1 2>&1)
echo "$BRANCH_OUT"
echo "$BRANCH_OUT" | grep -q "test result: ok" || fail "H5.5: thousand_branches_are_independent_and_deterministic FAILED"
pass "H5.5: thousand_branches_are_independent_and_deterministic — 1000+ branches forked from one Universe, each deterministic and non-perturbing"

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
echo "  - A universe with a forged cpu_signature is refused by restore on this real host unless"
echo "    a CPUID template is active"
echo "  - Multiverse::reset_dirty_pages rewinds only the RAM pages a KVM_CAP_DIRTY_LOG_RING ring"
echo "    reports as touched (a handful, never total RAM), and the rewind is byte-exact"
echo "  - Multiverse::branch forks 1000+ independent, deterministic continuations from one"
echo "    captured Universe (the spec's small-N fallback, not yet the memory-efficient"
echo "    UFFDIO_CONTINUE mechanism)"
echo ""
echo "H-series H0-H4 complete; H5's capture/restore round trip, CPU-mismatch refusal, dirty-ring"
echo "reset, and branch/fork determinism are now proven on real hardware. Remaining H5 work"
echo "(memory-efficient UFFDIO_CONTINUE branching, shell-into): see todo.md."
echo ""
