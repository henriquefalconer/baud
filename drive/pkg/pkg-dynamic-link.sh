#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg/pkg-dynamic-link.sh — a real dynamically-linked glibc /init, real /dev/kvm (todo.md §14
# item 1's H8 prerequisite: no dynamically-linked binary had ever booted through this pipeline, and
# InitramfsEntry had no symlink node type at all).
#
# Every existing fixture links statically via musl-gcc; a real distro/Buildroot/Nix rootfs (what H8
# — Super Mario Bros / FCEUX — will eventually need) is dynamically linked, and its dynamic linker
# is reached almost universally through a symlink (`/lib64/ld-linux-x86-64.so.2` -> a versioned path
# under `/lib/x86_64-linux-gnu/` on Debian/Ubuntu). This script drives the new `#[ignore]`d
# `guest_boots_a_dynamically_linked_glibc_init` test (`crates/baud-multiverse/src/linux/mod.rs`): it
# compiles a real, non-static glibc `/init` (`dynamic_init.c`), assembles an initramfs via
# `baud_packages::build_reproducible_initramfs` carrying this dev host's own real
# `ld-linux-x86-64.so.2` + `libc.so.6` (this host's glibc *is* the guest's glibc — identical x86_64
# Linux ABI, no cross-build needed) plus the `/lib64/...` symlink the binary's own `PT_INTERP`
# names, and boots it twice against the already-built `linux-guest` bzImage on real /dev/kvm — see
# `crates/baud-multiverse/tests/fixtures/linux-guest/BUILD.md`'s "dynamically-linked init" section
# for the full account.
#
# Needs gcc + this host's own /lib/x86_64-linux-gnu/{ld-linux-x86-64.so.2,libc.so.6} (both already
# present per CLAUDE.md's toolchain setup) — no kernel source tree, no kernel rebuild (reuses the
# checked-in bzImage). Opt-in like the other pkg-*.sh scripts, not part of the standard h0-h7 gate.

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[pkg-dynamic-link] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== baud-packages: dynamically-linked glibc /init, real boot ==="
echo ""

command -v gcc >/dev/null || fail "gcc not found on PATH"
[ -f /lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 ] || fail "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 not found on this host"
[ -f /lib/x86_64-linux-gnu/libc.so.6 ] || fail "/lib/x86_64-linux-gnu/libc.so.6 not found on this host"

log "Running guest_boots_a_dynamically_linked_glibc_init against real /dev/kvm..."
BOOT_OUT=$(cargo test -q -p baud-multiverse guest_boots_a_dynamically_linked_glibc_init -- --ignored --test-threads=1 2>&1)
echo "$BOOT_OUT"
echo "$BOOT_OUT" | grep -q "test result: ok" || fail "guest_boots_a_dynamically_linked_glibc_init FAILED"
echo "$BOOT_OUT" | grep -q "Skipping" && fail "guest_boots_a_dynamically_linked_glibc_init SKIPPED (gcc or host glibc files missing at test time)"
pass "guest_boots_a_dynamically_linked_glibc_init — a real, non-static glibc /init resolves ld.so + libc.so.6 out of a pipeline-built, symlink-carrying initramfs and boots twice on real /dev/kvm"

echo ""
echo "=== dynamically-linked glibc /init: PASSED ==="
echo ""
echo "The first dynamically-linked binary ever booted through baud-multiverse. InitramfsEntry's new"
echo "symlink support (crates/baud-packages/src/initramfs.rs) is exercised for real, not just in"
echo "unit tests: the /lib64/ld-linux-x86-64.so.2 symlink the compiled binary's own PT_INTERP names"
echo "resolves through the pipeline-built archive to a real ld-linux-x86-64.so.2, which in turn"
echo "resolves libc.so.6 via the binary's own DT_RUNPATH — the concrete shape a real glibc/"
echo "Buildroot/Nix rootfs (H8's eventual FCEUX + Lua harness image) will need."
