<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `tape-echo-guest` — H2's real bootable fixture for `all_input_is_tape_derived`

Same rationale and build mechanics as `../hello-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header `linux_loader::loader::bzimage::BzImage::load` accepts, no
kernel source tree/Nix/cross-compiler needed) — only the payload differs. Regenerate with
`python3 build.py` (needs only `as`/`ld`).

## What it does

`payload.s` is 20 bytes of hand-written x86-64: read exactly 4 bytes, one at a time, from the
tape device's `DATA` register (`in al, dx` at port `0x0500` —
`crates/baud-multiverse/src/tape_bus.rs`'s `TAPE_DEVICE_BASE + baud_tape_device::reg::DATA`, a
real single-byte `IN` instruction hitting the real PIO exit path `DeviceBus`/`TapeBus` serve),
and echo each byte straight back out to COM1 (`out dx, al` at port `0x3f8`, the same console
port `hello-guest` uses), then `hlt` in a loop.

No scheduler, no jiffies, no CPUID, no memory access beyond registers — same "subtractive rule"
minimalism as `hello-guest`, extended by exactly the one new thing this fixture exists to
exercise: a real guest instruction stream reading the tape device.

## Why this fixture exists

Before this fixture, `baud-tape-device`'s `all_input_is_tape_derived` test
(specs/baud-tape-device.md §5, test-matrix row 21 — "input not actually flowing from the tape
(fake determinism)") only existed at the pure device-model level: calling
`TapeDevice::pio_read`/`pio_write` directly, never through a real guest instruction executing
against a real KVM vCPU. `baud-multiverse`'s `DeviceBus`/`TapeBus` wiring (`tape_bus.rs`) was
therefore only type-checked, never exercised by an actual `IN`/`OUT` VM exit. This fixture closes
that gap the same way `hello-guest` closed H1's: a real, minimal guest whose only job is to prove
the property the spec names, executed against real `/dev/kvm`.

## The 4-byte length

`payload.s`'s `mov ecx, 4` must match `all_input_is_tape_derived`'s two 4-byte tapes
(`crates/baud-multiverse/src/linux/mod.rs`). Change one, change the other.
