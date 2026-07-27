#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h/h0.sh — H0 drive script: capability spike (specs/baud-host.md, todo.md §10)
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

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

log()  { echo "[h0] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }
info() { echo "  [INFO] $*"; }

echo ""
echo "=== H0: Capability Spike ==="
echo ""

log "Building workspace..."
# BAUD_GATE_PREBUILT: set by a gate that has already built the workspace, so the (~7s, target-dir
# locking) no-op `cargo build` below can be skipped when many drive scripts run concurrently.
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q -p baud-host -p baud-server -p baud-cli 2>&1
fi

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -t baud-h0-XXXXXX.sqlite)"
# Windows/git-bash: sqlite:// URIs need a native Windows path (posix /tmp/... is not
# understood by a plain win32 binary); cygpath -m gives a forward-slash Windows path.
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"

# Own port + own snapshot store, so this script can run concurrently with any other drive script.
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"
SNAP_ROOT="$(mktemp -d -t baud-h0-snap-XXXXXX)"

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
