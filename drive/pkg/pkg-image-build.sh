#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg/pkg-image-build.sh — automated guest-kernel build reproducibility (todo.md §4.5 / §14
# next-actions item 1)
#
# Exercises crates/baud-packages::kernel_build's #[ignore]d image_build_is_reproducible test for
# real: builds the linux-guest fixture's kernel (tests/fixtures/linux-guest/minimal.config) twice,
# from two independent scratch copies of the kernel source tree, and asserts the resulting
# bzImage bytes are byte-for-byte identical. This is the automated Rust equivalent of the by-hand
# recipe in tests/fixtures/linux-guest/BUILD.md's "Regenerating the kernel" section.
#
# Needs a real kernel source tree (CLAUDE.md: ~/wsl-kernel-src/src, or set BAUD_KERNEL_SRC) and
# gcc-13, and takes several minutes (two full kernel builds + two full-tree copies) — not part of
# the standard h0-h7 verification gate, same opt-in convention as the enforced-regime scripts
# (drive/manual/h3-enforced-rdtsc.sh etc).

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[pkg-image-build] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== baud-packages: automated guest-kernel build reproducibility ==="
echo ""

KERNEL_SRC="${BAUD_KERNEL_SRC:-$HOME/wsl-kernel-src/src}"
if [[ ! -f "$KERNEL_SRC/Makefile" ]]; then
    fail "no kernel source tree at $KERNEL_SRC (set BAUD_KERNEL_SRC) — see CLAUDE.md's \
'Building an out-of-tree kernel module' section"
fi
command -v gcc-13 >/dev/null || fail "gcc-13 not found on PATH — see CLAUDE.md"

log "Building baud-packages..."
cargo build -q -p baud-packages 2>&1

# /tmp on this dev host is a small tmpfs (a few GB, RAM-backed) — nowhere near enough for two
# copies of a kernel source tree plus their build output. Scratch onto the real disk instead
# (tempfile::tempdir() honors $TMPDIR).
export TMPDIR="$HOME/.baud-tmp"
mkdir -p "$TMPDIR"
trap 'rm -rf "$TMPDIR"' EXIT

log "Running image_build_is_reproducible against $KERNEL_SRC (builds a real kernel twice, ~2-5 min)..."
BUILD_OUT=$(BAUD_KERNEL_SRC="$KERNEL_SRC" cargo test -q -p baud-packages image_build_is_reproducible -- --ignored --test-threads=1 2>&1)
echo "$BUILD_OUT"
echo "$BUILD_OUT" | grep -q "test result: ok" || fail "image_build_is_reproducible FAILED"
echo "$BUILD_OUT" | grep -q "^Skipping" && fail "image_build_is_reproducible SKIPPED (no kernel tree / gcc-13 found at test time)"
pass "image_build_is_reproducible: two independent from-source builds of the linux-guest kernel produce a byte-identical bzImage"

echo ""
echo "=== baud-packages guest-kernel build pipeline: PASSED ==="
echo ""
