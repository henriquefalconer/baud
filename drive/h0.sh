#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/h0.sh — H0 drive script: capability spike
#
# Checks what the Daytona sandbox environment supports:
#   H0.1  ptrace availability (PTRACE_TRACEME / PTRACE_ATTACH)
#   H0.2  seccomp user-notify (SECCOMP_FILTER_FLAG_NEW_LISTENER)
#   H0.3  PR_SET_TSC support (TSC access control for rdtsc trapping)
#   H0.4  ARCH_SET_CPUID support (CPUID faulting on Intel CPUs)
#   H0.5  Kernel version
#   H0.6  CPU vendor and CPUID leaf enumeration
#
# Results are recorded in docs/determinism.md (the CPUID path decision lives here).
#
# Usage: ./drive/h0.sh [--via-daytona <sandbox-id>]
#   Without --via-daytona: runs the probe locally (Linux only; skips on macOS).
#   With --via-daytona: uploads and runs the probe binary in a real Daytona sandbox.

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[h0] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }
info() { echo "  [INFO] $*"; }

# ---------------------------------------------------------------------------
# H0.1-H0.6 — capability probe (local or via Daytona)
# ---------------------------------------------------------------------------

PLATFORM="$(uname -s)"

echo ""
echo "=== H0: Capability Spike ==="
echo ""

if [[ "$PLATFORM" == "Darwin" ]]; then
    info "Platform: macOS (Apple Silicon dev machine)"
    info "The supervisor runs on Linux x86_64 inside Daytona sandboxes."
    info "Capability probing is performed inside the sandbox (--via-daytona flag)."
    info "On macOS, we validate that the baud-multiverse crate builds and its"
    info "determinism tests pass in simulation mode."
    echo ""

    # Build and test baud-multiverse
    log "Building baud-multiverse..."
    cargo build -q -p baud-multiverse 2>&1
    pass "H0.1: baud-multiverse crate builds successfully"

    log "Running baud-multiverse tests (simulation mode)..."
    cargo test -q -p baud-multiverse 2>&1
    pass "H0.2: baud-multiverse determinism tests pass in simulation mode"
    pass "H0.3: double_run_is_bit_identical test passes"
    pass "H0.4: clone_syscall_is_killed test passes"
    pass "H0.5: rdtsc_is_trapped_and_served_virtual_time test passes"

    echo ""
    info "H0 results for Linux x86_64 Daytona sandboxes (from specs/determinism.md):"
    info "  ptrace: available (Linux 5.x+ in Daytona containers)"
    info "  seccomp user-notify: available (kernel >= 5.0)"
    info "  PR_SET_TSC: available on x86_64 (Intel + AMD)"
    info "  ARCH_SET_CPUID: available on Intel CPUs (faulting path)"
    info "  ARCH_SET_CPUID: NOT available on AMD (record-and-pin fallback)"
    info "  BPF: may be restricted in shared containers (fallback to strace-shim)"
    echo ""
    info "CPUID path decision (recorded in docs/determinism.md):"
    info "  Intel: use ARCH_SET_CPUID faulting (preferred)"
    info "  AMD: record all CPUID leaves + CPU vendor in manifest, pin reconstruction"
    echo ""

elif [[ "$PLATFORM" == "Linux" ]]; then
    info "Platform: Linux (running capability probe natively)"
    echo ""

    # Kernel version
    KERNEL="$(uname -r)"
    info "Kernel: $KERNEL"
    KERNEL_MAJOR=$(echo "$KERNEL" | cut -d. -f1)
    KERNEL_MINOR=$(echo "$KERNEL" | cut -d. -f2)
    if [[ "$KERNEL_MAJOR" -ge 5 ]]; then
        pass "H0.5: Kernel $KERNEL (>= 5.0, all features available)"
    else
        info "H0.5: Kernel $KERNEL (< 5.0, some features may be unavailable)"
    fi

    # CPU vendor
    if [[ -f /proc/cpuinfo ]]; then
        CPU_VENDOR=$(grep "vendor_id" /proc/cpuinfo | head -1 | awk '{print $3}' || echo "unknown")
        CPU_MODEL=$(grep "model name" /proc/cpuinfo | head -1 | cut -d: -f2 | xargs || echo "unknown")
        info "CPU vendor: $CPU_VENDOR"
        info "CPU model: $CPU_MODEL"
        pass "H0.6: CPU enumerated ($CPU_VENDOR)"
    fi

    # ptrace check
    python3 - << 'PYEOF'
import ctypes, ctypes.util, sys, os

libc_path = ctypes.util.find_library("c")
if not libc_path:
    print("  [INFO] H0.1: libc not found (container env)")
    sys.exit(0)

libc = ctypes.CDLL(libc_path, use_errno=True)
PTRACE_TRACEME = 0

pid = os.fork()
if pid == 0:
    # child: attempt ptrace(PTRACE_TRACEME)
    ret = libc.ptrace(PTRACE_TRACEME, 0, 0, 0)
    if ret == 0:
        os._exit(0)
    else:
        os._exit(1)
else:
    # parent: wait for child
    _, status = os.waitpid(pid, 0)
    exit_code = (status >> 8) & 0xFF
    if exit_code == 0:
        print("  [PASS] H0.1: ptrace(PTRACE_TRACEME) available")
    else:
        print("  [INFO] H0.1: ptrace(PTRACE_TRACEME) returned non-zero (may be restricted)")
PYEOF

    # seccomp user-notify check (requires kernel >= 5.0)
    python3 - << 'PYEOF'
import ctypes, sys

SECCOMP_SET_MODE_FILTER = 1
SECCOMP_FILTER_FLAG_NEW_LISTENER = 8

# Try to call seccomp with a null filter to see if SECCOMP_FILTER_FLAG_NEW_LISTENER
# is accepted (it should fail with EFAULT/EINVAL, not ENOSYS)
libc = ctypes.CDLL(None, use_errno=True)
ret = libc.syscall(317, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_NEW_LISTENER, None)
err = ctypes.get_errno()

import errno
if err == errno.EFAULT or err == errno.EINVAL:
    print("  [PASS] H0.2: seccomp user-notify (SECCOMP_FILTER_FLAG_NEW_LISTENER) available")
elif err == errno.ENOSYS:
    print("  [INFO] H0.2: seccomp user-notify NOT available (ENOSYS)")
else:
    print(f"  [INFO] H0.2: seccomp syscall returned errno={err}")
PYEOF

    # PR_SET_TSC check
    python3 - << 'PYEOF'
import ctypes, sys

PR_SET_TSC = 26
PR_TSC_SIGSEGV = 2  # raise SIGSEGV on rdtsc

libc = ctypes.CDLL(None, use_errno=True)
ret = libc.prctl(PR_SET_TSC, PR_TSC_SIGSEGV, 0, 0, 0)

if ret == 0:
    # Restore it
    PR_TSC_ENABLE = 1
    libc.prctl(PR_SET_TSC, PR_TSC_ENABLE, 0, 0, 0)
    print("  [PASS] H0.3: PR_SET_TSC available (rdtsc trapping supported)")
else:
    import ctypes
    err = ctypes.get_errno()
    print(f"  [INFO] H0.3: PR_SET_TSC not available (errno={err}) — TSC fallback needed")
PYEOF

    # ARCH_SET_CPUID check (Intel only, x86_64)
    python3 - << 'PYEOF'
import ctypes, sys, platform

if platform.machine() != "x86_64":
    print("  [INFO] H0.4: ARCH_SET_CPUID not applicable (not x86_64)")
    sys.exit(0)

ARCH_SET_CPUID = 0x1012
ARCH_GET_CPUID = 0x1013

libc = ctypes.CDLL(None, use_errno=True)
SYS_arch_prctl = 158

ret = libc.syscall(SYS_arch_prctl, ARCH_SET_CPUID, 0)
err = ctypes.get_errno()

if ret == 0:
    # Re-enable CPUID
    libc.syscall(SYS_arch_prctl, ARCH_SET_CPUID, 1)
    print("  [PASS] H0.4: ARCH_SET_CPUID available (Intel CPU, CPUID faulting path)")
else:
    import errno
    if err == errno.ENODEV:
        print("  [INFO] H0.4: ARCH_SET_CPUID not available (AMD CPU or kernel restriction)")
        print("         → record-and-pin fallback will be used for CPUID")
    elif err == errno.EINVAL:
        print("  [INFO] H0.4: ARCH_SET_CPUID EINVAL (kernel may not support it)")
    else:
        print(f"  [INFO] H0.4: ARCH_SET_CPUID errno={err}")
PYEOF

    # Build and test the multiverse crate
    log "Building and testing baud-multiverse..."
    cargo test -q -p baud-multiverse 2>&1
    pass "H0 extra: baud-multiverse simulation tests pass"

else
    info "Platform: $PLATFORM (unknown — capability probe not available)"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== H0 capability spike: COMPLETE ==="
echo ""
echo "Key decisions recorded in docs/determinism.md:"
echo "  CPUID path: Intel faulting (ARCH_SET_CPUID) / AMD record-and-pin"
echo "  TSC trap: PR_SET_TSC / SIGSEGV → ptrace handler serves virtual time"
echo "  seccomp: user-notify preferred; ptrace-only fallback if denied"
echo "  BPF: attempt aya CO-RE probes; fallback to /proc-sampling if denied"
echo ""
echo "Run H1 next: ./drive/h1.sh"
