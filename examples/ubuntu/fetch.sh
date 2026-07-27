#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# examples/ubuntu/fetch.sh -- download, SHA256-verify, and prep the real Ubuntu 18.04.1 LTS cloud
# image + kernel + initrd for H9 (todo.md §14 item 8-11, specs/baud-ubuntu.md). Idempotent: re-running
# skips any artifact already present and verified.
#
# specs/baud-ubuntu.md §4 asks for "the cloud-images-archive.ubuntu.com build whose /etc/os-release
# reads PRETTY_NAME=\"Ubuntu 18.04.1 LTS\"". cloud-images.ubuntu.com/releases/18.04/release/ is a
# ROLLING alias that now serves 18.04.6 (the latest point release respin), not the original 18.04.1
# build -- confirmed by downloading it and reading /etc/os-release directly. The dated snapshot
# release-20180806 (the first respin after 18.04.1 shipped on 2018-07-26) is confirmed via the same
# check to report exactly PRETTY_NAME="Ubuntu 18.04.1 LTS" and /etc/issue = "Ubuntu 18.04.1 LTS \n \l"
# (the exact three-token banner form §4's last bullet asks for) -- this script pins that dated build.
#
# Output artifacts are NOT checked into git (the raw rootfs alone is ~2.2 GiB) -- this script writes
# them to $OUT_DIR (default ~/.baud-tmp/ubuntu-1804, override with --out-dir), the same
# outside-the-repo-tree convention CLAUDE.md already documents for ~/wsl-kernel-src.

set -euo pipefail

BUILD=release-20180806
BASE_URL="https://cloud-images.ubuntu.com/releases/bionic/${BUILD}"
OUT_DIR="${OUT_DIR:-$HOME/.baud-tmp/ubuntu-1804}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

log() { echo "[fetch-ubuntu] $*" >&2; }

mkdir -p "$OUT_DIR"
cd "$OUT_DIR"

log "output dir: $OUT_DIR (build: $BUILD)"

fetch_and_verify() {
    local url="$1" out="$2" sums_file="$3"
    if [[ -f "$out" ]]; then
        local existing
        existing="$(sha256sum "$out" | cut -d' ' -f1)"
        local expected
        expected="$(grep " \*${out}\$" "$sums_file" | cut -d' ' -f1)"
        if [[ "$existing" == "$expected" ]]; then
            log "$out already present and verified, skipping"
            return
        fi
        log "$out present but checksum mismatch, re-downloading"
    fi
    log "downloading $out"
    curl -sS -L -o "$out" "$url"
    local got expected
    got="$(sha256sum "$out" | cut -d' ' -f1)"
    expected="$(grep " \*${out}\$" "$sums_file" | cut -d' ' -f1)"
    if [[ "$got" != "$expected" ]]; then
        echo "SHA256 mismatch for $out: got $got, expected $expected" >&2
        exit 1
    fi
    log "$out verified ($got)"
}

curl -sS -L -o SHA256SUMS.qcow2 "$BASE_URL/SHA256SUMS"
curl -sS -L -o SHA256SUMS.unpacked "$BASE_URL/unpacked/SHA256SUMS"

fetch_and_verify "$BASE_URL/ubuntu-18.04-server-cloudimg-amd64.img" \
    ubuntu-18.04-server-cloudimg-amd64.img SHA256SUMS.qcow2
fetch_and_verify "$BASE_URL/unpacked/ubuntu-18.04-server-cloudimg-amd64-vmlinuz-generic" \
    vmlinuz-generic SHA256SUMS.unpacked
fetch_and_verify "$BASE_URL/unpacked/ubuntu-18.04-server-cloudimg-amd64-initrd-generic" \
    initrd-generic SHA256SUMS.unpacked

# specs/baud-ubuntu.md §4's "one-time image prep": convert qcow2 -> raw, then disable mount-count /
# interval fsck (`tune2fs -c 0 -i 0`) so a real boot never triggers a e2fsck rewrite of the journal
# (the cloud image ships with an already-clean, already-unmounted ext4 journal -- confirmed via
# `dumpe2fs -h`'s "Filesystem state: clean" -- so this step is defence in depth, not a fixup).
if [[ ! -f rootfs.raw ]] || [[ "ubuntu-18.04-server-cloudimg-amd64.img" -nt rootfs.raw ]]; then
    log "converting qcow2 -> raw"
    qemu-img convert -O raw ubuntu-18.04-server-cloudimg-amd64.img rootfs.raw

    log "pinning mount-count/check-interval on the root partition (tune2fs -c 0 -i 0)"
    LOOP="$(sudo losetup -fP --show rootfs.raw)"
    trap 'sudo losetup -d "$LOOP" 2>/dev/null || true' EXIT
    sudo tune2fs -c 0 -i 0 "${LOOP}p1" >/dev/null
    sudo losetup -d "$LOOP"
    trap - EXIT
fi

log "done. rootfs.raw / vmlinuz-generic / initrd-generic are ready in $OUT_DIR"
