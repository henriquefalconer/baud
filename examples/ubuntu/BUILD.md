<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `examples/ubuntu` — fetching the real Ubuntu 18.04.1 LTS artifacts

H9 (todo.md §14 items 8-11, `specs/baud-ubuntu.md`) boots a **real, unmodified** Ubuntu 18.04.1 LTS
distro, not a hand-built fixture. The artifacts are large (the raw rootfs alone is ~2.2 GiB) and are
**not checked into git** — run `bash examples/ubuntu/fetch.sh` to download, SHA256-verify, and prep
them into `~/.baud-tmp/ubuntu-1804` (override with `--out-dir`), the same outside-the-repo-tree
convention `CLAUDE.md` already documents for `~/wsl-kernel-src`.

## Which build, exactly

`specs/baud-ubuntu.md` §4 asks for the build whose `/etc/os-release` reads
`PRETTY_NAME="Ubuntu 18.04.1 LTS"`. `cloud-images.ubuntu.com/releases/18.04/release/` is a **rolling**
alias that serves the latest 18.04.x point-release respin — as of this writing that's 18.04.6, not
18.04.1 (confirmed by downloading it and reading `/etc/os-release` directly). The dated snapshot
`release-20180806` (the first respin after 18.04.1 shipped on 2018-07-26) is confirmed, the same way,
to report exactly `PRETTY_NAME="Ubuntu 18.04.1 LTS"` and `/etc/issue = "Ubuntu 18.04.1 LTS \n \l"` —
the exact three-token banner form §4's last bullet names. `fetch.sh` pins this exact dated build
(`https://cloud-images.ubuntu.com/releases/bionic/release-20180806/`).

## What `fetch.sh` produces

- `vmlinuz-generic` — the stock 4.15 kernel, SHA256-verified against the build's own `SHA256SUMS`.
- `initrd-generic` — the stock initrd (carries `virtio_pci`/`virtio_blk`), same verification.
- `rootfs.raw` — the qcow2 cloud image converted to raw (`qemu-img convert -O raw`), with
  `tune2fs -c 0 -i 0` applied to the root partition (spec §4's "one-time image prep" — defence in
  depth; the cloud image already ships with a clean, unmounted ext4 journal per `dumpe2fs -h`).

## Real-hardware finding: a genuine `layout::initramfs_load_addr` bug this image exposed

Booting this real kernel+initrd through the existing `baud run kvm --initramfs ... --acpi
--virtio-blk-image ...` path (every flag already wired end-to-end before this image existed, todo.md
§14 items 5/8-11) hit a real crash the very first time a full-size distro kernel was tried:
`Initramfs unpacking failed: junk in compressed archive`, then a page fault in `free_reserved_area`.
Root-caused and fixed in `crates/baud-multiverse/src/layout.rs`'s `initramfs_load_addr` — see that
function's own doc comment for the full mechanism (the fixed 32 MiB `INITRAMFS_ADDR` collided with
this kernel's own `init_size`-driven self-decompression scratch space) and todo.md §14 for the
iteration-by-iteration narrative.

## Manually attempting a boot

```bash
bash examples/ubuntu/fetch.sh   # one-time, ~5 min (mostly the 2.2 GiB qcow2->raw convert)

# from the repo root, with a baud-server already running (see any drive/*.sh for the pattern):
UBUNTU_CMDLINE="console=ttyS0 nokaslr nosmp maxcpus=1 clocksource=tsc tsc=reliable no-kvmclock \
  no_timer_check reboot=t panic=-1 quiet loglevel=1 random.trust_cpu=off random.trust_bootloader=on \
  i8042.noaux i8042.nomux i8042.nopnp 8250.nr_uarts=1 systemd.unit=multi-user.target \
  cloud-init=disabled root=/dev/vda1 ro rootwait net.ifnames=0 biosdevname=0 scsi_mod.scan=sync \
  udev.children_max=1 fsck.mode=skip systemd.mask=systemd-timesyncd.service"

baud run kvm \
  --kernel ~/.baud-tmp/ubuntu-1804/vmlinuz-generic \
  --initramfs ~/.baud-tmp/ubuntu-1804/initrd-generic \
  --virtio-blk-image ~/.baud-tmp/ubuntu-1804/rootfs.raw \
  --acpi \
  --cmdline "$UBUNTU_CMDLINE" \
  --periodic-timer-period-rcb 500000 \
  --periodic-timer-max-ticks 3000 \
  --tape-hex "" \
  --json
```

**Confirmed so far** (real `/dev/kvm`, this iteration): ACPI tables parse cleanly (`ACPI: Core
revision ...`), PCI enumerates via the legacy `0xCF8/0xCFC` mechanism, the initramfs unpacks
successfully (the bug above, fixed), and the boot reaches `Freeing unused kernel memory` — the very
end of kernel init, immediately before handing off to `/init`.

**Still open, the next thing to attempt**: the run stops there because
`Multiverse::run_to_first_halt_with_periodic_timer` (the primitive `boot_run_and_drain` uses) returns
on the *first* guest-issued `Hlt`, and a real multi-tasking kernel's idle loop calls `hlt` the moment
nothing is runnable — which happens almost immediately once `/init` blocks on its first disk read,
long before systemd, `agetty`, or the `ubuntu login:` banner. Raising `--periodic-timer-max-ticks`
from 200 to 3000 produced **byte-identical console output** in this iteration, confirming this is a
real halt, not a truncation — reaching login needs a different run-loop primitive (survive/resume
past an idle halt, or "run until console contains `ubuntu login:`"), not a bigger tick budget on the
existing one.
