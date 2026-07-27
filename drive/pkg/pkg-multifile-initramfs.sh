#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg/pkg-multifile-initramfs.sh — pipeline-built multi-file initramfs, real /dev/kvm (todo.md
# §4.3/§4.5, §14 next-actions item 1's "no real harness-script/agent-binary multi-file rootfs has
# been assembled or tested yet" gap)
#
# Every existing fixture's initramfs was hand-`cpio`'d with exactly one file (`/init`); the real
# Rust pipeline (`baud_packages::build_reproducible_initramfs`, already wired end-to-end through
# `baud image build --initramfs-entry`) had never been exercised with more than one distinct entry.
# This script drives the new `#[ignore]`d `guest_boots_a_pipeline_built_multi_file_initramfs` test
# (`crates/baud-multiverse/src/linux/mod.rs`): it builds a 2-file initramfs (`/init` execs a
# bundled `/helper`) via that pipeline at test time, then boots it twice against the already-built
# `linux-guest` bzImage on real /dev/kvm — see
# `crates/baud-multiverse/tests/fixtures/linux-guest/BUILD.md`'s "pipeline-built multi-file
# initramfs" section for the full account.
#
# Needs musl-gcc only (no kernel source tree, no kernel rebuild — reuses the checked-in bzImage) —
# fast, but still opt-in like the other pkg-*.sh scripts, not part of the standard h0-h7 gate.

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[pkg-multifile-initramfs] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== baud-packages: pipeline-built multi-file initramfs, real boot ==="
echo ""

command -v musl-gcc >/dev/null || fail "musl-gcc not found on PATH — see CLAUDE.md"

log "Running guest_boots_a_pipeline_built_multi_file_initramfs against real /dev/kvm..."
BOOT_OUT=$(cargo test -q -p baud-multiverse guest_boots_a_pipeline_built_multi_file_initramfs -- --ignored --test-threads=1 2>&1)
echo "$BOOT_OUT"
echo "$BOOT_OUT" | grep -q "test result: ok" || fail "guest_boots_a_pipeline_built_multi_file_initramfs FAILED"
echo "$BOOT_OUT" | grep -q "Skipping" && fail "guest_boots_a_pipeline_built_multi_file_initramfs SKIPPED (musl-gcc missing at test time)"
pass "guest_boots_a_pipeline_built_multi_file_initramfs — a 2-file initramfs assembled by build_reproducible_initramfs boots twice on real /dev/kvm, both bundled files' markers present each boot"

echo ""
echo "=== pipeline-built multi-file initramfs: PASSED ==="
echo ""
echo "build_reproducible_initramfs's multi-file capacity (already wired through 'baud image build"
echo "--initramfs-entry', repeatable) is now real-hardware-verified: a pipeline-built archive with"
echo "/init + a second bundled /helper binary boots on real KVM, and /init successfully execs the"
echo "bundled second file — the concrete shape a real multi-file rootfs (e.g. the eventual §11"
echo "harness + emulator pair) will need."
