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

## Real-hardware finding: `--periodic-timer-vector` must be `238` (`0xee`), not the `236` default

The default `--periodic-timer-vector` (`0xec`) is `LOCAL_TIMER_VECTOR` for the newer ~6.18 kernel
this project's other fixtures (`tests/fixtures/linux-guest/`) use — **not** for the real Ubuntu
18.04.1 image's stock 4.15 kernel. Direct source lookup (`arch/x86/include/asm/irq_vectors.h` at
`github.com/torvalds/linux` tag `v4.15`) confirms 4.15's `LOCAL_TIMER_VECTOR` is `0xee` (238); `0xec`
is an ordinary unclaimed device-IRQ vector in that layout. Booting with the `236` default produces an
endless `do_IRQ: 0.236 No irq handler for vector` loop with guest jiffies visibly stuck (confirmed by
temporarily dropping `quiet loglevel=1` for full verbose boot output) — every injected tick lands on
the generic do-nothing IRQ dispatch, never the real `apic_timer_interrupt` ISR, so the guest never
legitimately reschedules. With `--periodic-timer-vector 238`, the same verbose boot reaches `Freeing
unused kernel memory` with **zero** `do_IRQ` errors, confirming the real timer ISR is now being
dispatched correctly. (An earlier same-session test that *seemed* to show vector `238` producing zero
console output at all, even after resuming past many idle halts, turned out to be a red herring: with
`quiet loglevel=1` still on the cmdline, boot output is suppressed for *both* vectors up to the first
halt — the emptiness said nothing about vector correctness by itself. Always verify a vector change
against a verbose (no `quiet`, no `loglevel=1`) boot first.)

## Manually attempting a boot

```bash
bash examples/ubuntu/fetch.sh   # one-time, ~5 min (mostly the 2.2 GiB qcow2->raw convert)

# from the repo root, with a baud-server already running (see any drive/*.sh for the pattern):
UBUNTU_CMDLINE="console=ttyS0 nokaslr nosmp maxcpus=1 clocksource=tsc tsc=reliable no-kvmclock \
  no_timer_check reboot=t panic=-1 quiet loglevel=1 random.trust_cpu=off random.trust_bootloader=on \
  i8042.noaux i8042.nomux i8042.nopnp 8250.nr_uarts=1 systemd.unit=multi-user.target \
  cloud-init=disabled root=/dev/vda1 ro rootwait net.ifnames=0 biosdevname=0 scsi_mod.scan=sync \
  udev.children_max=1 fsck.mode=skip systemd.mask=systemd-timesyncd.service"

PATTERN_HEX=$(echo -n "ubuntu login:" | xxd -p | tr -d '\n')

baud run kvm \
  --kernel ~/.baud-tmp/ubuntu-1804/vmlinuz-generic \
  --initramfs ~/.baud-tmp/ubuntu-1804/initrd-generic \
  --virtio-blk-image ~/.baud-tmp/ubuntu-1804/rootfs.raw \
  --acpi \
  --cmdline "$UBUNTU_CMDLINE" \
  --periodic-timer-period-rcb 500000 \
  --periodic-timer-vector 238 \
  --periodic-timer-max-ticks 20000 \
  --halt-console-pattern-hex "$PATTERN_HEX" \
  --halt-max-exits-per-burst 200000 \
  --tape-hex "" \
  --json
```

**Confirmed so far** (real `/dev/kvm`): ACPI tables parse cleanly (`ACPI: Core revision ...`), PCI
enumerates via the legacy `0xCF8/0xCFC` mechanism, the initramfs unpacks successfully (the bug above,
fixed), and — with the correct `--periodic-timer-vector 238` — the boot reaches `Freeing unused kernel
memory` with a clean, error-free kernel log, the very end of kernel init, immediately before handing
off to `/init`. `Multiverse::run_until_console_pattern_with_periodic_timer_and_devices` (the
resume-past-idle-halt primitive `--halt-console-pattern-hex` selects, todo.md §14 item 13) is wired to
carry the boot past that first halt.

**Still open, the next thing to attempt**: reaching the `ubuntu login:` banner itself. With the
correct vector, the resume-past-halt engine visibly does *real* per-tick work (each resumption costs
much more wall-clock than vector `236`'s cheap infinite `do_IRQ` spin — consistent with genuine
disk I/O / systemd activity rather than a dead loop), but a 20000-tick attempt did not finish within an
interactive session's time budget. The next iteration should run this recipe with `run_in_background:
true` and a long budget (many minutes), watching server-side CPU (`ps`) rather than only the
synchronous HTTP response, and/or add a progress log (e.g. periodic `console_output().len()` polling
via a second endpoint) so a long-running attempt is observable without waiting for it to fully
complete or time out.
