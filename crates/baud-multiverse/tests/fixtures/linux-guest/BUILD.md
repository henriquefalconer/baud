<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `linux-guest` — a real, unmodified Linux kernel booting through baud (todo.md §14 item 1)

Unlike every other fixture in this directory (`hello-guest`, `timer-guest`, ...), `bzImage` here is
a **real, compiled Linux 6.18 kernel** — not a hand-assembled payload wrapped in a minimal
`setup_header`. It boots through baud-multiverse's real KVM boot flow (H1's loader + H4's periodic
interrupt injection) all the way to a real `/init` process, which prints a marker and cleanly powers
off — the first time this project has booted an actual, unmodified Linux kernel to userspace
(todo.md §14, `guest_kernel_boots_to_userspace`).

`initramfs.cpio.gz` bundles one statically-linked binary (`/init`, built from `init.c` in this
directory) as PID 1.

## Regenerating the kernel (`bzImage`)

Needs a Linux kernel source tree and `gcc-13` (CLAUDE.md's "Building an out-of-tree kernel module"
section already documents installing `gcc-13` to match this dev host's kernel build toolchain — the
same compiler works fine for a from-scratch guest kernel; nothing here needs to match the *host*
kernel's own build, only `gcc-13`'s availability). This project's dev host happens to already have a
kernel source tree checked out at `~/wsl-kernel-src/src` for the enforced-`kvm_intel` module work
(CLAUDE.md) — **do not build the guest kernel there**: that tree has host-module build artifacts
in-tree (not `O=`-separable) and applied `kvm_intel` enforcement patches unrelated to a guest kernel.
Copy it to a scratch location first:

```
cp -a ~/wsl-kernel-src/src ~/baud-guest-kernel-src && cd ~/baud-guest-kernel-src
make CC=gcc-13 mrproper                          # this copy has no in-tree build artifacts to keep
make CC=gcc-13 allnoconfig
./scripts/kconfig/merge_config.sh -m .config <this-dir>/minimal.config
make CC=gcc-13 olddefconfig
make CC=gcc-13 -j$(nproc) bzImage                # ~65s on this dev host; output at arch/x86/boot/bzImage
cp arch/x86/boot/bzImage <this-dir>/bzImage
```

`minimal.config` is a fragment implementing spec §4.1's required/disabled list (`allnoconfig` base +
this fragment + `olddefconfig` to resolve dependencies) — notably **`CONFIG_X86_IOPL_IOPERM=y`**,
without which `/init`'s `iopl(3)` call below faults immediately (a real bug this fixture's own
first real-hardware boot caught, todo.md §14). Two Kconfig symbols in the spec's disable list cannot
actually be turned off on x86_64 and are harmless as compiled-in-but-inert: `HPET_TIMER` (`def_bool
X86_64` in `arch/x86/Kconfig`, stays inert with no ACPI table to announce its MMIO address) and
`X86_LOCAL_APIC`-adjacent code (mandatory on x86_64; the kernel correctly detects "No local APIC
present" against baud's open-bus MMIO fallback and disables APIC facility — see "Why no LAPIC device
model was needed" below).

## Regenerating the initramfs

Only needs `musl-gcc` and `cpio` (`sudo apt-get install -y cpio` if missing) — no kernel source tree:

```
musl-gcc -static -Os -o init init.c && strip init
mkdir -p rootfs && cp init rootfs/init && chmod 755 rootfs/init
cd rootfs && touch -h -d '@1' init . && \
    find . -print0 | sort -z | cpio -o -H newc -R +0:+0 --reproducible --null | gzip -9n \
    > ../initramfs.cpio.gz
```
(§4.3's exact reproducible-cpio recipe — fixed mtimes, sorted entries, uid/gid 0, `gzip -9n`.)

## `entropy_init.c` / `entropy_initramfs.cpio.gz` — H7 `os_entropy_is_deterministic`

A second `/init`, for the *same already-built* `bzImage` above (no kernel rebuild: OS-entropy
determinism is entirely a userspace-visible property this fixture's `minimal.config` already
supports, via `CONFIG_DEVTMPFS_MOUNT=y` giving `/dev/urandom` for free). It calls `getrandom()`
four times and reads `/dev/urandom` four times, hex-encoding each 32-byte read and writing it out
through the same raw-`outb` COM1 endpoint (see "Why `/init` uses raw port I/O" below — same
reasoning applies). Regenerate its initramfs the same way as the main one, just from
`entropy_init.c`:

```bash
musl-gcc -static -Os -o entropy_init entropy_init.c && strip entropy_init
mkdir -p entropy_rootfs && cp entropy_init entropy_rootfs/init && chmod 755 entropy_rootfs/init
cd entropy_rootfs && touch -h -d '@1' init . && \
    find . -print0 | sort -z | cpio -o -H newc -R +0:+0 --reproducible --null | gzip -9n \
    > ../entropy_initramfs.cpio.gz
```

Only `entropy_init.c` and `entropy_initramfs.cpio.gz` are checked in (same convention as the main
fixture: the compiled `entropy_init` binary and `entropy_rootfs/` build directory are not).

## Why `/init` uses raw port I/O, not `write(1, ...)`

`init.c` writes its marker via `iopl(3)` + inline `outb` straight to COM1's data register (`0x3f8`)
— todo.md §4.4's option A ("userspace PIO ... zero driver, interrupt-free") — rather than a plain
`write(1, marker, len)`. This is load-bearing, not a style choice: this machine has no interrupt
controller (`Using NULL legacy PIC` in the boot log, since baud never emulates one), so the 8250
UART's normal *interrupt-driven* tty transmit path (`write` → line discipline → enable `THRI` → wait
for a real IRQ4 that never fires) never drains. The kernel's own `printk` output still reaches the
console because it uses a separate, polled console-write path that never depends on an interrupt —
which is exactly why boot messages appear fine while an early version of this fixture's `/init`
(using plain `write(1, ...)`) produced a clean boot-to-shutdown with **no marker in the console
output at all**. Raw `outb` sidesteps the tty layer (and its interrupt dependency) entirely.

## Why no LAPIC device model was needed

A real x86_64 Linux kernel cannot disable its local-APIC code paths (mandatory on this arch), so it
was an open question whether booting one against baud-multiverse — which has no `KVM_CREATE_IRQCHIP`
and no LAPIC MMIO device at all — would require building one. It did not: with no memory region
registered at the LAPIC's fixed MMIO base, every register access falls through to baud's existing
generic open-bus fallback (`OPEN_BUS_BYTE` = `0xFF`, `console.rs`/`tape_bus.rs`) exactly like any
other unclaimed address. The kernel probes the LAPIC ID register, reads back `0xFFFFFFFF`, correctly
concludes "No local APIC present", and falls back to `Using NULL legacy PIC` — booting on with no
APIC at all. `Multiverse::run_to_first_halt_with_periodic_timer`'s H4 injection engine
(`KVM_INTERRUPT`) still delivers the chosen vector straight into the vCPU's IDT dispatch regardless
of any virtualized APIC (the same mechanism the hand-assembled `timer-guest` fixture already
exercises), so Linux's `LOCAL_TIMER_VECTOR` (`0xec`, `arch/x86/include/asm/irq_vectors.h`) ISR still
runs on each injected tick — logged as `Spurious LAPIC timer interrupt on cpu 0` (Linux's own
"correct" response to a LOCAL_TIMER_VECTOR interrupt when it does not believe it owns a LAPIC), which
is harmless: it does not need to *believe* the tick is real, only to keep taking the vCPU off `HLT`
periodically so timekeeping (`jiffies`/`calibrate_delay`) and the scheduler make forward progress.

## Two real bugs this fixture's first real boot caught (beyond the two above)

1. **`baud-vcpu`'s `IrqWindowOpen` exit was mapped to `Exit::Unmodeled`** (`crates/baud-vcpu/src/
   linux/mod.rs`), so any guest that genuinely needed `boundary::PmuStepper::run_until_irq_window`'s
   request-interrupt-window fallback (not already injectable the instant `inject_at` checked) hit the
   run-loop's determinism-hole catch-all instead of ever reaching that fallback's own readiness
   check. Every fixture before this one happened to stay injectable throughout (interrupts never
   genuinely disabled for long); a real kernel's early boot — which disables interrupts for real
   stretches — is the first guest to force this path. Fixed: `Exit::IrqWindowOpen` is now a proper
   variant `dispatch_exit` resolves to `Continue` (a control signal, not a device access with a value
   to serve), covered by a new unit test (`irq_window_open_continues_rather_than_faulting`).
2. **e820 left the entire first megabyte `reserved`** (`crates/baud-multiverse/src/linux/
   bootparams.rs`), but Linux's `reserve_real_mode()`/`init_real_mode()` unconditionally **panics**
   ("Real mode trampoline was not allocated") if it cannot find sub-1MiB free memory for the
   AP-bringup/ACPI-resume real-mode trampoline — unreachable by any hand-assembled fixture, since
   none of them ever run that kernel code path. Fixed: `layout::LOW_MEM_RAM_START` (`0x1000`) plus a
   low-memory `usable` e820 entry between it and `HIMEM_START` (page zero stays `reserved`, the
   conventional real-mode IVT/BDA carve-out); safe because the kernel copies everything it still
   needs (`boot_params`, the command line) into its own compiled-in memory, and switches off baud's
   bootstrap identity page tables onto its own static ones, both during early 64-bit entry — well
   before `memblock`/e820 parsing ever runs.

## A known, deliberate non-goal: full double-boot console/RAM byte-equality

`guest_kernel_boots_to_userspace` (this fixture's test, `linux/mod.rs`) checks that **each** boot
independently reaches `/init`, prints the marker, and halts cleanly, and that the two boots take the
same number of periodic ticks — but it does **not** assert the two boots' console output or RAM hash
are byte-identical. A first attempt at that stricter check found the two boots' console text differs
in exactly one line: `kernel/sched/clock.c`'s `sched_clock: Marking stable (A, B)->(C, 0)`, which
prints raw TSC-derived nanosecond values at the exact moment a kernel-internal heuristic transitions
— a transition point sensitive to this project's already-documented small real-hardware
branch-counter read imprecision (`RCB_HARDWARE_JITTER_TOLERANCE`, `timer_tick_lands_at_identical_
instruction`'s doc), visibly amplified into differing printed numbers even though the underlying
instruction stream is otherwise identical. This is precisely why todo.md's own H7 spec for
`double_boot_ram_hash_identical` calls for hashing RAM at a **guest-driven checkpoint** (an explicit
`outb`/hypercall the workload issues) rather than at a wall-clock point or over raw console text —
that stricter, checkpoint-based comparison is real Linux guest work still open for H7, not part of
this milestone's contract.
