<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `shell-guest` — H5's real bootable fixture for `shell_into_universe_resumes`

Same rationale and build mechanics as `../hello-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header `linux_loader::loader::bzimage::BzImage::load` accepts, no
kernel source tree/Nix/cross-compiler needed) — only the payload differs. Regenerate with
`python3 build.py` (needs only `as`/`ld`).

## What it does

`payload.s` is a tiny hand-written x86-64 "shell": print a `$ ` prompt to COM1, then loop forever
polling the Line Status Register (`in al, dx` at port `0x3fd`, bit 0 = "data ready") until a byte
is available, read it from the DATA register (port `0x3f8`), and either echo it straight back
(any byte except carriage return) or, on `\r` (0x0D), print a newline and re-print the prompt.
There is no `hlt` anywhere in this fixture, deliberately — a real interactive shell never exits on
its own, so this fixture's own run loop never reaches `Hlt`/`Shutdown` either; a caller drives it
with `Multiverse::step_exit`/`run_until_console_len` (`crates/baud-multiverse/src/linux/mod.rs`)
instead of `run_to_first_halt`.

No scheduler, no jiffies, no CPUID, no memory access beyond registers — same "subtractive rule"
minimalism as every other hand-assembled fixture here, extended by exactly the one new thing this
fixture exists to exercise: a real guest instruction stream that *reads* from the console (the RX
half of `Console`/`vm_superio::Serial`, `crates/baud-multiverse/src/console.rs`'s
`enqueue_input`/`Serial::enqueue_raw_bytes`), not just writes to it.

## Why this fixture exists

specs/baud-snapshot.md §5's "restore into a live shell" step and its named test
`shell_into_universe_resumes` need a guest that can be captured mid-interaction, restored, and
then keep taking live input and producing live output — none of the existing fixtures
(`hello-guest`, `tape-echo-guest`, `timer-guest`, `rdrand-guest`) read anything from the console,
so none of them could exercise `Console::enqueue_input`/`Multiverse::enqueue_console_input`
against a real guest instruction stream. This fixture closes that gap the same way `hello-guest`
closed H1's and `tape-echo-guest` closed H2's: a real, minimal guest whose only job is to prove the
property the spec names, executed against real `/dev/kvm`.

## The polling protocol (why LSR, not an interrupt)

`Console`'s UART model (`crates/baud-multiverse/src/console.rs`) still uses `NoIrqTrigger` — a
recording-but-non-delivering `vm_superio::Trigger` — because this workspace has no in-kernel LAPIC
(`KVM_CREATE_IRQCHIP` is never called; H4's interrupt-injection engine delivers interrupts
directly via `KVM_INTERRUPT`, see `crates/baud-multiverse/src/linux/mod.rs`'s
`inject_timer_tick`). Rather than build a second, IRQ4-specific injection path just for this
fixture, `shell-guest` polls LSR the same way real 16550 driver code does when it is not using
interrupts at all — `vm_superio::Serial::enqueue_raw_bytes` already sets the LSR "data ready" bit
directly (independent of whether the trigger fires), so a polling guest observes queued input
correctly with no interrupt-delivery machinery required. A future fixture that blocks on IRQ4
instead would need a real `EventFd`-backed `Trigger`, noted as future work in `console.rs`'s
module doc.
