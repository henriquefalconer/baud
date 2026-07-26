#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg-build-cli.sh — `baud image build` end-to-end, through the real CLI/server (todo.md
# §4.5 / §14 next-actions item 1: "no `baud image build` command exists yet ... neither
# initramfs.rs nor kernel_build.rs is callable from any real caller yet").
#
# drive/pkg-image-build.sh already proves `baud_packages::kernel_build::build_bzimage` itself is
# reproducible (two from-source builds, byte-identical bzImage). This script instead proves the
# *wiring* around it: a real `baud image build` CLI invocation against a real running
# `baud-server`, using the exact kernel-source-tree + config-fragment + single-file-initramfs
# recipe `crates/baud-multiverse/tests/fixtures/linux-guest/BUILD.md` documents by hand, ending in
# a real `bzImage` + `initramfs.cpio.gz` pair plus spec §4.5's image identity hash.
#
# Needs a real kernel source tree (CLAUDE.md: ~/wsl-kernel-src/src, or set BAUD_KERNEL_SRC),
# gcc-13, and musl-gcc, and takes several minutes (one full kernel build + one full-tree copy) —
# not part of the standard h0-h7 gate, same opt-in convention as drive/pkg-image-build.sh.

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$PATH"

log()  { echo "[pkg-build-cli] $*" >&2; }
pass() { echo "  [PASS] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

echo ""
echo "=== baud-packages: 'baud image build' CLI/server end-to-end ==="
echo ""

KERNEL_SRC="${BAUD_KERNEL_SRC:-$HOME/wsl-kernel-src/src}"
if [[ ! -f "$KERNEL_SRC/Makefile" ]]; then
    fail "no kernel source tree at $KERNEL_SRC (set BAUD_KERNEL_SRC) — see CLAUDE.md's \
'Building an out-of-tree kernel module' section"
fi
command -v gcc-13 >/dev/null || fail "gcc-13 not found on PATH — see CLAUDE.md"
command -v musl-gcc >/dev/null || fail "musl-gcc not found on PATH (needed to build the fixture's /init)"

log "Building baud-server/baud-cli/baud-packages..."
cargo build -q -p baud-server -p baud-cli -p baud-packages 2>&1

# /tmp on this dev host is a small tmpfs (a few GB, RAM-backed) — nowhere near enough for a
# kernel-source-tree copy plus its build output (same finding drive/pkg-image-build.sh made).
export TMPDIR="$HOME/.baud-tmp"
mkdir -p "$TMPDIR"
SCRATCH_KERNEL="$(mktemp -d)"
OUTPUT_DIR="$(mktemp -d)"
INIT_DIR="$(mktemp -d)"

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
DB_FILE="$(mktemp -u -t baud-pkg-build-cli-XXXXXX.sqlite)"
FIXTURE_DIR="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/linux-guest"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -rf "$SCRATCH_KERNEL" "$OUTPUT_DIR" "$INIT_DIR" "$TMPDIR" "$DB_FILE" 2>/dev/null || true
}
trap cleanup EXIT

log "Copying kernel source tree to disposable scratch (never build in the shared tree)..."
cp -a "$KERNEL_SRC/." "$SCRATCH_KERNEL"

log "Compiling the linux-guest fixture's /init with musl-gcc..."
musl-gcc -static -Os -o "$INIT_DIR/init" "$FIXTURE_DIR/init.c"
strip "$INIT_DIR/init"

log "Starting baud-server..."
pkill -f "baud-server" 2>/dev/null || true; sleep 0.2
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" "$BAUD_SERVER_BIN" &
SERVER_PID=$!
sleep 1

log "baud image build (real kernel build, ~4-5 min)..."
BUILD_JSON="$("$BAUD" image build \
    --kernel-src "$SCRATCH_KERNEL" \
    --config-fragment "$FIXTURE_DIR/minimal.config" \
    --cc gcc-13 \
    --initramfs-entry "init:755:$INIT_DIR/init" \
    --output-dir "$OUTPUT_DIR" \
    --json)" || fail "'baud image build --json' FAILED to run"
echo "$BUILD_JSON"

OK="$(echo "$BUILD_JSON" | grep -oE '"ok":[[:space:]]*(true|false)' | grep -oE 'true|false')"
[[ "$OK" == "true" ]] || fail "'baud image build' reported ok=false"
pass "'baud image build' reported ok=true"

[[ -s "$OUTPUT_DIR/bzImage" ]] || fail "output bzImage missing or empty at $OUTPUT_DIR/bzImage"
pass "bzImage written to $OUTPUT_DIR/bzImage ($(stat -c%s "$OUTPUT_DIR/bzImage") bytes)"

[[ -s "$OUTPUT_DIR/initramfs.cpio.gz" ]] || fail "output initramfs.cpio.gz missing or empty"
pass "initramfs.cpio.gz written to $OUTPUT_DIR/initramfs.cpio.gz ($(stat -c%s "$OUTPUT_DIR/initramfs.cpio.gz") bytes)"

IMAGE_HASH="$(echo "$BUILD_JSON" | grep -oE '"image_hash":[[:space:]]*"[0-9a-f]+"' | grep -oE '[0-9a-f]{64}')"
[[ -n "$IMAGE_HASH" ]] || fail "response missing a 64-hex-char image_hash"
pass "image_hash present: $IMAGE_HASH"

# Boot verification of this exact kernel+initramfs recipe is already covered on real /dev/kvm by
# guest_kernel_boots_to_userspace (drive/h7.sh) against the hand-built fixture bzImage/initramfs —
# same kernel_src/config_fragment/init.c recipe this script drives through the CLI/server, so a
# byte-identical bzImage (already proven reproducible by drive/pkg-image-build.sh) is expected
# here too. `baud run kvm --initramfs ... --periodic-timer-period-rcb ...` now exists
# (drive/pkg-boot-cli.sh boots the checked-in fixture pair through it end-to-end) — booting this
# script's own freshly-built OUTPUT_DIR/{bzImage,initramfs.cpio.gz} through it directly, instead
# of only the checked-in fixture copy, is still open future work.

echo ""
echo "=== baud image build CLI/server wiring: PASSED ==="
echo ""
echo "POST /image/build composes baud_packages::build_bzimage + build_reproducible_initramfs"
echo "into one real bzImage + initramfs.cpio.gz pair, reachable end-to-end from 'baud image"
echo "build --json' against a real running baud-server."
