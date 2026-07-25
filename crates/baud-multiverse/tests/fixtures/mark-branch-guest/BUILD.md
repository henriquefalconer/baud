<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `mark-branch-guest` — the first fixture that keeps running after a branch point

Same rationale and build mechanics as `../hello-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header `linux_loader::loader::bzimage::BzImage::load` accepts, no
kernel source tree/Nix/cross-compiler needed) — only the payload differs. Regenerate with
`python3 build.py` (needs only `as`/`ld`).

## What it does

`payload.s` loops 4 times: read one byte from the tape device's `DATA` register (`in al, dx` at
port `0x0500`), echo it to COM1 (`out dx, al` at port `0x3f8`, same as `tape-echo-guest`), then
issue the tape device's `MARK_BRANCH` control op (`mov al, 1` / `out dx, al` at the `CONTROL`
port `0x0508` — `crates/baud-tape-device/src/lib.rs`'s `ControlOp::MarkBranch = 1`, no payload
bytes needed first). After the fourth iteration it `hlt`s in a loop, same as every other fixture.

## Why this fixture exists — closing the "every fixture halts on tape exhaustion" gap

Every fixture before this one (`hello-guest`, `tape-echo-guest`, `timer-guest`, `shell-guest`,
`rdrand-guest`) reads exactly the tape it's given and then halts — todo.md's own "M-series sixth
brick" entry documents that this makes forking an already-halted branch with a fresh tape suffix
a **provable no-op**: the vCPU is stuck at `Hlt` and never reads anything new, so
`run_to_first_halt()` on the fork just replays the original branch's frozen output, completely
independent of the new suffix (confirmed live in that entry: asked for a new 4-byte suffix, got
back the *original* branch's bytes).

`mark-branch-guest` is the fixture that closes that gap: it emits `MARK_BRANCH` **and keeps
running afterward** (reads more tape, does more work), four separate times, giving
`baud-multiverse`'s new `Multiverse::run_until_branch_or_halt` primitive something real to stop
at, and giving a branch forked from that stopping point real, tape-driven work left to do —
proving that resuming a mid-run checkpoint with a new tape suffix genuinely changes the guest's
subsequent output, not just its frozen final state.

## The 4-iteration length

`payload.s`'s `mov ecx, 4` is a free choice (unlike `tape-echo-guest`'s, which must match a fixed
test tape length) — four iterations was picked only to have more than one `MARK_BRANCH` point
available, so a test can prove chaining across *two* checkpoints, not just one. A test using N
bytes of tape only needs `N <= 4`; the loop always emits one `MARK_BRANCH` per byte consumed, so
the Kth `MARK_BRANCH`'s `step` is always exactly `K` (the tape cursor after reading exactly K
bytes, `crates/baud-proto/src/lib.rs`'s `Msg::MarkBranch { step }` convention).
