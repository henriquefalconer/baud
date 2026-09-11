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

# The final raw-image checks need loop-device and mount privileges. Use a non-interactive sudo
# invocation so a headless drive fails with an actionable prerequisite instead of hanging on a
# terminal prompt. Prime the credential once before calling this script, as documented in CLAUDE.md.
sudo_cmd() {
    if [[ -n "${BAUD_SUDO_PASSWORD:-}" ]]; then
        printf '%s\n' "$BAUD_SUDO_PASSWORD" | sudo -S -p '' "$@"
    else
        sudo -n "$@"
    fi
}

mkdir -p "$OUT_DIR"
cd "$OUT_DIR"

log "output dir: $OUT_DIR (build: $BUILD)"

fetch_and_verify() {
    local url="$1" out="$2" sums_file="$3"
    # The SHA256SUMS files list the *remote* basename (e.g.
    # "ubuntu-18.04-server-cloudimg-amd64-vmlinuz-generic"), not `$out`'s shortened local name
    # ("vmlinuz-generic") -- grepping on `$out` directly (as this function used to) never matches
    # for vmlinuz-generic/initrd-generic (only the qcow2 image's `$out` happens to equal its own
    # remote name), so `expected` always came back empty and the whole script died silently right
    # here under `set -eo pipefail` (grep's exit 1 on no-match propagating through the command
    # substitution), before ever logging a mismatch -- found for real re-running this script after
    # an interrupted first attempt had already downloaded (and correctly verified, since that
    # first attempt's `[[ -f "$out" ]]` was false and it happened to exit during download, before
    # ever reaching this comparison) vmlinuz-generic.
    local remote_name
    remote_name="$(basename "$url")"
    if [[ -f "$out" ]]; then
        local existing
        existing="$(sha256sum "$out" | cut -d' ' -f1)"
        local expected
        expected="$(grep " \*${remote_name}\$" "$sums_file" | cut -d' ' -f1)"
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
    expected="$(grep " \*${remote_name}\$" "$sums_file" | cut -d' ' -f1)"
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
    LOOP="$(sudo_cmd losetup -fP --show rootfs.raw)"
    trap 'sudo_cmd losetup -d "$LOOP" 2>/dev/null || true' EXIT
    sudo_cmd tune2fs -c 0 -i 0 "${LOOP}p1" >/dev/null
    sudo_cmd losetup -d "$LOOP"
    trap - EXIT
fi

# Validate the prepared filesystem, not just the download hashes. A cloud image with the wrong
# point release or a dirty journal can still have perfectly valid SHA256 values and then boot a
# different userspace than H9 claims to prove.
log "validating Ubuntu release metadata and clean filesystem state"
if ! sudo_cmd true 2>/dev/null; then
    echo "Ubuntu artifact validation needs cached sudo credentials; run 'echo baud | sudo -S -v' first or set BAUD_SUDO_PASSWORD" >&2
    exit 1
fi
LOOP_VALIDATE="$(sudo_cmd losetup -fP --show rootfs.raw)"
MOUNT_VALIDATE="$(mktemp -d)"
cleanup_validate() {
    sudo_cmd umount "$MOUNT_VALIDATE" 2>/dev/null || true
    sudo_cmd losetup -d "$LOOP_VALIDATE" 2>/dev/null || true
    rmdir "$MOUNT_VALIDATE" 2>/dev/null || true
}
trap cleanup_validate EXIT
sudo_cmd mount -o ro "${LOOP_VALIDATE}p1" "$MOUNT_VALIDATE"
grep -Fx 'PRETTY_NAME="Ubuntu 18.04.1 LTS"' "$MOUNT_VALIDATE/etc/os-release" >/dev/null \
    || { echo "rootfs is not Ubuntu 18.04.1 LTS" >&2; exit 1; }
grep -F 'Ubuntu 18.04.1 LTS' "$MOUNT_VALIDATE/etc/issue" >/dev/null \
    || { echo "rootfs /etc/issue does not identify Ubuntu 18.04.1 LTS" >&2; exit 1; }
sudo_cmd umount "$MOUNT_VALIDATE"
sudo_cmd tune2fs -l "${LOOP_VALIDATE}p1" | grep -E 'Filesystem state:[[:space:]]+clean' >/dev/null \
    || { echo "rootfs filesystem is not clean" >&2; exit 1; }
cleanup_validate
trap - EXIT

# Keep a machine-readable identity beside the external artifacts. H9 compares this exact build
# across VM boots, so a directory with a silently replaced rootfs must never look ready.
python3 - "$OUT_DIR" "$BUILD" <<'PY'
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])

def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

manifest = {"build": sys.argv[2], "artifacts": {}}
for name in ("rootfs.raw", "vmlinuz-generic", "initrd-generic"):
    path = root / name
    manifest["artifacts"][name] = {"bytes": path.stat().st_size, "sha256": sha256(path)}
(root / "manifest.json").write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
PY

log "done. rootfs.raw / vmlinuz-generic / initrd-generic / manifest.json are ready in $OUT_DIR"
