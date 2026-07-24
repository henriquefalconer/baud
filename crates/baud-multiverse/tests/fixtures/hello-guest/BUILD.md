<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `hello-guest` — H1's real bootable fixture

`bzImage` in this directory is **not** the full guest-image contract baud-packages will eventually
build from a `spec.toml` via pinned Nix (specs/baud-packages.md §9; that pipeline does not exist
yet, todo.md §14) — it is a small, purpose-built, checked-in test fixture that lets
`double_boot_memory_identical` (specs/baud-multiverse.md §3.1) run for real against `/dev/kvm`,
the same way `linux-loader`/`kvm-ioctls` themselves ship tiny compiled `.bin` fixtures in their own
test suites rather than building a kernel at test time.

Regenerate it with `python3 build.py` (needs only `as`/`ld`, already required to build this
workspace's KVM-linked crates — no kernel source tree, no Nix, no cross-compiler).

## Why a hand-built payload, not a real Linux kernel

The first version of this fixture *was* a real Linux kernel (Ubuntu `linux-source-7.0.0`,
`make tinyconfig` + just enough config to run a tiny static-musl init). It got real further than
expected — booting it against actual `/dev/kvm` for the first time surfaced three genuine,
previously-unexercised production bugs (see "bugs this fixture caught" below) — but a real Linux
kernel needs a working scheduler tick to ever reach `reboot()`/halt: `calibrate_delay()` and the
early scheduler both spin waiting for `jiffies` to advance, which only happens once a timer
interrupt is actually being delivered. `baud-multiverse`'s run loop does not inject interrupts yet
(that lands with H4, specs/baud-multiverse.md §3.4's arm-early-then-single-step engine — built in
`baud-vcpu::boundary`, not yet wired into this crate's boot flow) — so a real kernel hangs forever
waiting for a tick that H1, by design, does not send. Full distro-kernel boot is the right target
once H4 lands; H1's own spec ("boot a minimal guest kernel that prints to the serial console;
clean `Hlt`/`Shutdown`") only asks for a payload that prints and halts, so `hello-guest` now *is*
exactly that: no scheduler, no jiffies, no timer dependency at all.

`payload.s` is 17 bytes of hand-written x86-64 (`.intel_syntax noprefix`), assembled with
`as`/`ld` into a flat binary with no ELF headers or sections: write `msg` byte-by-byte to
COM1's data register (port `0x3f8`, `out dx, al` — direct port I/O, no UART-driver setup needed,
since `console.rs`'s line-status register already always reports "ready to transmit"), then `hlt`
in a loop. `build.py` wraps that flat binary in the minimum `setup_header` `linux_loader::loader::
bzimage::BzImage::load` actually validates:

| Offset | Field | Value | Why |
|---|---|---|---|
| `0x1F1` | `setup_sects` | `4` | Defines `setup_size = (sects+1)*512 = 2560` — where the "protected-mode kernel" (our payload) starts in the file. `BzImage::load` defaults a `0` to `4` too; set explicitly for clarity. |
| `0x1FE` | `boot_flag` | `0xAA55` | Traditional x86 boot-sector magic; not itself checked by `BzImage::load`, but part of the same protocol our own `bootparams.rs` re-asserts after loading. |
| `0x202` | `header` | `0x53726448` ("HdrS") | `BzImage::load` returns `Error::InvalidBzImage` if this does not match exactly — the one magic-number check that gates "is this even a bzImage". |
| `0x206` | `version` | `0x0200` | Must be `>= 0x0200` or the loader rejects the image as "too old a protocol". |
| `0x211` | `loadflags` | `0x01` | Bit 0 (`LOADED_HIGH`) must be set; the loader checks `loadflags & 0x1 == 0` and rejects otherwise. |
| `0x214` | `code32_start` | `0x00200000` | Must be `>= HIMEM_START` (`0x100000`) — `bootparams.rs` passes `Some(HIMEM_START)` as the loader's `highmem_start_address`, and the loader validates the file's own `code32_start` against it *before* overwriting it with the real load address. |

Every other header byte is zero: `BzImage::load` never reads them, and the payload itself ignores
`RSI` (which would normally point at `boot_params`) entirely — there is no real Linux kernel here
to hand a zero page to.

The payload is placed at file offset `setup_size + 0x200` (`ENTRY_OFFSET` in `build.py`): baud sets
`RIP = kernel_load + layout::KERNEL_64BIT_ENTRY_OFFSET` (`0x200`) unconditionally after loading, so
the 512 bytes between `setup_size` and the payload are loaded into guest RAM but never executed —
`build.py` pads them with zeros rather than omitting them.

## The marker byte string

`payload.s`'s `msg` must stay byte-for-byte identical to `crates/baud-multiverse/src/linux/mod.rs`'s
`HELLO_GUEST_MARKER` constant — `double_boot_memory_identical` asserts the guest's console output
equals it exactly. Change one, change the other.

## Bugs this fixture caught on its first real run

Booting *some* payload against real `/dev/kvm` for the first time (this project's first-ever real
KVM boot, both with the original Linux-kernel version of this fixture and this one) surfaced three
production bugs that had been type-checked but never executed since the whole KVM/VT-x pivot began
(todo.md §14 catalogs the "not yet exercised on real hardware" gap this fixture closes):

1. `linux::configure_msr_filter` set an empty `MsrFilterRangeFlags` and an "allow" bitmap bit —
   the kernel rejects `flags == 0` outright (`KVM_X86_SET_MSR_FILTER`,
   `arch/x86/kvm/x86.c:kvm_add_msr_filter`), and even past that, an "allow" bit would have let
   `IA32_TSC`/`_DEADLINE`/`_AUX` accesses proceed in-kernel instead of reaching the VMM's
   work-clock. Fixed: `READ | WRITE` flags with a "deny" bitmap bit (which, with
   `KVM_MSR_EXIT_REASON_FILTER` enabled, is what actually routes the access to userspace).
2. `pagetables::long_mode_sregs` left `TR` at an all-zero `kvm_segment` (`present = 0`,
   `unusable` also `0`) — VMX's guest-state checks require TR to always be present with a valid
   busy-TSS type (its "unusable" bit is reserved, unlike every other segment register), so
   VM-entry failed outright (`KVM_EXIT_FAIL_ENTRY`, `hardware_entry_failure_reason =
   EXIT_REASON_INVALID_STATE = 0x21`). Fixed: a minimal valid busy-32-bit-TSS `TR` descriptor;
   `LDTR` explicitly marked `unusable` (legal for LDTR, unlike TR).
3. (Real-Linux-kernel version of this fixture only, still a live gap in `cpuid.rs` today) a stock
   kernel hung forever polling PIT channel 2 (port `0x42`) inside `quick_pit_calibrate()`/
   `native_calibrate_cpu_early()` — this machine has no PIT to serve. Fixed by synthesizing CPUID
   leaves `15H` (TSC/crystal-clock ratio) and `16H` (processor frequency) in `cpuid.rs`'s
   determinism mask table to a fixed value matching `VIRTUAL_TSC_KHZ`, and by adding a minimal
   deterministic CMOS RTC shim (`console.rs`'s `Cmos`, ports `0x70`/`0x71`) after the *same* kernel
   then hung polling the RTC's "Update In Progress" bit via the open-bus fallback's fixed `0xFF`
   (all bits set, so UIP always read "busy"). This fixture's current hand-written payload never
   reaches either code path (no CPUID, no CMOS access), but both fixes remain load-bearing for any
   future real-kernel fixture and are exercised by their own unit tests
   (`cpuid.rs`/`console.rs`).

None of these were reachable by `cargo check --target x86_64-unknown-linux-gnu` (all are runtime
behaviors of real hardware/kernel code, not type errors) — only a real boot against real `/dev/kvm`
surfaced them.
