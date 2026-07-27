<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `spin-guest` — a guest that causes zero VM exits, ever

Same rationale and build mechanics as `../hello-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header `linux_loader::loader::bzimage::BzImage::load` accepts, no
kernel source tree/Nix/cross-compiler needed) — only the payload differs. Regenerate with
`python3 build.py` (needs only `as`/`ld`).

## What it does

`payload.s` is exactly `1: jmp 1b` — no `in`/`out`/`hlt`, no I/O, no interrupts, nothing that
could ever trap. Once the vCPU enters guest mode it never leaves it on its own.

## Why this fixture exists — proving the wall-clock watchdog actually works

Every other fixture in this directory reaches `Hlt`/`Shutdown` (or issues `MARK_BRANCH`) sooner or
later, so `Multiverse::run_to_first_halt`/`run_until_halted` always returns given enough real time.
todo.md §14.1 "Still open" item 1 documented that, before the wall-clock watchdog existed,
`baud_vcpu::linux::run_until_halted` was a bare unbounded `loop`: a guest that made no VM exits at
all — trivially possible under this project's subtractive machine model, which has no APIC/PIT/
host interrupts to force one — hung the vCPU thread forever. `spin-guest` is the fixture that
proves the fix: it is the one guest image in this repo with **no way out** except the watchdog's
own `pthread_kill`-forced `EINTR` (`crates/baud-vcpu/src/linux/watchdog.rs`). A test driving it
with a short `Multiverse::set_watchdog_budget` and asserting `RunLoopError::WatchdogKilled` comes
back within a bounded wall-clock window is the only way to exercise that code path at all — every
other fixture halts before the watchdog would ever fire.
