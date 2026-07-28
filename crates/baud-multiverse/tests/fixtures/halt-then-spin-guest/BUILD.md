<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `halt-then-spin-guest` — a guest that halts once, then never exits again after waking

Same rationale and build mechanics as `../timer-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header, an IDT with one real gate at vector `0x30`, `sti`) — only the
payload differs. Regenerate with `python3 build.py` (needs only `as`/`ld`/`objcopy`/`nm`).

## What it does

`payload.s` runs `lidt`/`sti`/`hlt`, then falls straight into `spin: jmp spin` — reached only via
the injected interrupt's `iretq`, which resumes execution at the exact point `hlt` was
interrupted. The ISR itself writes one `'T'` marker byte to COM1 before returning, so the first
interrupt delivered after the halt still causes one ordinary VM exit; every VM entry after that
retires zero conditional branches and causes no further exit, ever — the same "no way out"
property `../spin-guest/payload.s` has, but reached *after* a real `Hlt` and one delivered
interrupt instead of from a cold start.

## Why this fixture exists — proving the resume-past-halt burst loop's own watchdog works

`Multiverse::run_to_first_halt_with_periodic_timer_and_devices`'s "no device has pending work, but
the caller wants to keep going until the console shows `p`" branch
(`crates/baud-multiverse/src/linux/mod.rs`) delivers the next periodic-timer interrupt directly and
then drains VM exits one `step_exit_cancellable` call at a time until the guest halts again or the
pattern appears. Every existing periodic-timer test passes `pattern = None`, which returns *before*
this loop is ever reached — so before this fixture, nothing in the test suite exercised it at all,
even though a real H9 (Ubuntu 18.04.1) boot attempt exercises exactly this loop (`pattern =
Some("ubuntu login:")`) and, per todo.md §14 item 17's `gdb`-backtrace-confirmed finding, is where a
genuine unbounded stall actually lived — a code path distinct from `inject_at`, which the
periodic-tick watchdog already covered. `halt_then_spin_burst_watchdog_kills_a_wedged_burst_exit`
(`crates/baud-multiverse/src/linux/mod.rs`) drives this fixture with `pattern` set to bytes that
never appear and a short per-call watchdog budget, asserting `RunLoopError::WatchdogKilled` comes
back within a bounded wall-clock window instead of hanging — the burst-loop equivalent of
`../spin-guest/BUILD.md`'s own rationale for the tick-level and whole-run watchdogs.
