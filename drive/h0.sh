#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h0.sh — H0 drive script: capability spike (specs/baud-host.md, todo.md §10)
#
# `baud host probe --json` asserts every capability baud-multiverse needs before any guest boots:
#   /dev/kvm + VT-x, CPUID control, TSC stability, MSR filtering, single-step, branch-counter
#   determinism, nested-virt, CPU vendor, and whether the host is `runnable` (any determinism at
#   all) / `enforced_capable` (hardware-traps RDTSC/RDRAND/#UD too).
#
# A failing capability is reported, never hidden (exit 1 for a non-runnable host) — this drive
# passes either way, because *reporting the truth* is what H0 verifies, not that this particular
# machine happens to have real KVM.

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h0] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }
info() { echo "  [INFO] $*"; }

echo ""
echo "=== H0: Capability Spike ==="
echo ""

log "Building workspace..."
cargo build -q -p baud-host -p baud-server -p baud-cli 2>&1

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h0-XXXXXX.sqlite)"
# Windows/git-bash: sqlite:// URIs need a native Windows path (posix /tmp/... is not
# understood by a plain win32 binary); cygpath -m gives a forward-slash Windows path.
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
set +e
PROBE_JSON="$("$BAUD" host probe --json)"
PROBE_EXIT=$?
set -e
echo "$PROBE_JSON"

# H0.1 — every capability field is present (never a partial / silently-missing probe).
for field in kvm vmx cpuid tsc_stable msr_filter singlestep rcb_deterministic nested vendor \
             enforced_module_present runnable enforced_capable capacity; do
    if ! echo "$PROBE_JSON" | grep -q "\"$field\""; then
        fail "H0.1: probe JSON missing field '$field'"
    fi
done
pass "H0.1: probe reports every capability field"

# H0.2 — enforced_capable never overclaims: it can only be true when runnable is also true.
RUNNABLE="$(echo "$PROBE_JSON" | grep -oE '"runnable":[[:space:]]*(true|false)' | grep -oE 'true|false')"
ENFORCED_CAPABLE="$(echo "$PROBE_JSON" | grep -oE '"enforced_capable":[[:space:]]*(true|false)' | grep -oE 'true|false')"
if [[ "$ENFORCED_CAPABLE" == "true" && "$RUNNABLE" != "true" ]]; then
    fail "H0.2: enforced_capable=true but runnable=false — an overclaim"
fi
pass "H0.2: runnable='$RUNNABLE' enforced_capable='$ENFORCED_CAPABLE' (enforced_capable never overclaims)"

# H0.3 — a non-runnable host names the failing check, and the CLI reports it as a real failure
# (exit 1) rather than a silent/false pass; a capable host reports success.
if [[ "$RUNNABLE" != "true" ]]; then
    if ! echo "$PROBE_JSON" | grep -q '"reason":[[:space:]]*"[^"]'; then
        fail "H0.3: non-runnable host did not name a reason"
    fi
    pass "H0.3: non-runnable host names the failing check ($(echo "$PROBE_JSON" | grep -o '"reason":[[:space:]]*"[^"]*"'))"
    if [[ "$PROBE_EXIT" -eq 0 ]]; then
        fail "H0.3: 'baud host probe' exited 0 on a non-runnable host (must be a real failure, exit 1)"
    fi
    pass "H0.3: CLI exits 1 on a non-runnable host (never a false pass)"
    info "This host cannot run baud-multiverse: KVM/VT-x is unavailable here. See docs/determinism.md."
else
    if [[ "$PROBE_EXIT" -ne 0 ]]; then
        fail "H0.3: 'baud host probe' exited non-zero despite runnable='$RUNNABLE'"
    fi
    pass "H0.3: CLI exits 0 with runnable='$RUNNABLE'"
fi

echo ""
echo "=== H0 capability spike: COMPLETE ==="
echo "Result recorded in docs/determinism.md — runnable='$RUNNABLE' enforced_capable='$ENFORCED_CAPABLE'."
echo ""
echo "Run baud-host's own unit tests for the capability-decision logic (hardware-independent):"
echo "  cargo test -p baud-host"
echo ""
echo "Next: H1 boots a guest and needs runnable=true on the deploy host (Linux + /dev/kvm)."
