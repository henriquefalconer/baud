<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `framebuffer-guest` — the first real guest to exercise the tape device's `FRAME` opcode

Same rationale and build mechanics as `../rdtsc-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header, no kernel source tree/Nix/cross-compiler needed) — only the
payload differs. Regenerate with `python3 build.py` (needs only `as`/`ld`).

## What it does

`payload.s` is hand-written x86-64: write one marker byte (`'F'`) to COM1 (port `0x3f8`), then
write a 2x2 `Indexed8` frame to the tape device (port `0x0500`, `baud-tape-device`'s `reg::DATA`) —
format byte `2`, width `2` and height `2` as little-endian `u32`s, then the four raw pixel bytes
`10, 20, 30, 40` — then finalize the record by writing opcode `5` (`ControlOp::Frame`) to the
control port (`0x0508`, `reg::CONTROL`), then `hlt` in a loop.

## Why this fixture exists

todo.md §14's "Not yet done" list had carried "the framebuffer stream" as open since the M-series
crate map first described `baud-stream` capturing a real guest's display — but `baud-proto::Msg`
already had a `Frame(FrameRecord)` variant, and `baud-stream` itself (frame fingerprinting, QOI/Y4M
encoding) was already built and unit-tested; the missing piece was that **no real device ever
produced a `Frame` record** — `baud-tape-device::ControlOp` only had `Probe`/`MarkBranch`/`Goal`/
`Violation`/`Log`, and no guest fixture wrote to the tape device to request one.

The natural fix is *not* a new VGA/virtio-gpu device — specs/baud-multiverse.md's own non-goal
("real device emulation beyond the console + tape device") rules that out, and
specs/baud-stream.md's own display-adapter contract already describes exactly this shape: "the
guest (or its bridge fixture) writes length-prefixed raw frame buffers ... the supervisor's device
model delivers them to this crate" — the tape device already *is* that device model, the same way
it already carries `LOG`/`PROBE` records. So the fix was a new `ControlOp::Frame` opcode
(`baud-tape-device/src/lib.rs`), and this fixture is the first real guest that uses it — the
transport-level analogue of what `mark-branch-guest` was for `MARK_BRANCH` and what `tape-echo-guest`
was for tape reads.

## What it proves

`linux::tests::framebuffer_guest_frame_is_reproducible_across_boots` boots this fixture twice on an
empty tape (it never reads the tape device, only writes to it) and asserts the single drained
`Msg::Frame` record — width, height, format, pixel bytes, and blake3 hash — is byte-identical
across both boots: specs/baud-stream.md §7's own named test (`frame_hashes_double_run_identical`)
run for the first time against a real guest on real `/dev/kvm`, not just the crate-level synthetic
buffers `baud-stream`'s own unit tests already covered.
