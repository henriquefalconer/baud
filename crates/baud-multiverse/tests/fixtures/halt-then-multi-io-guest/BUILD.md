<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `halt-then-multi-io-guest` — a guest that, once woken from halt, causes three separate VM exits before spinning forever

Same rationale and build mechanics as `../halt-then-spin-guest/BUILD.md` (hand-assembled flat
binary wrapped in a minimal bzImage header, an IDT with one real gate at vector `0x30`, `sti`) —
only the payload differs. Regenerate with `python3 build.py` (needs only `as`/`ld`/`objcopy`/`nm`).

## What it does

`payload.s` runs `lidt`/`sti`/`hlt`, same as `../halt-then-spin-guest/payload.s`. But where that
fixture's ISR itself writes the one COM1 marker byte before `iretq`, this fixture's ISR does
nothing but `iretq` — all three marker writes (`'A'`, `'B'`, `'C'`) happen in the *resumed* main
flow, each its own `out` instruction and therefore its own VM exit, before falling into `final:
jmp final` (the same zero-further-exits terminal state every other spin fixture in this directory
uses).

## Why this fixture exists — proving the resume-past-halt burst loop services devices between raw exits, not just once per tick

`Multiverse::run_to_first_halt_with_periodic_timer_and_devices`'s resume-past-halt burst loop
(`crates/baud-multiverse/src/linux/mod.rs`) used to check `devices` only before entering the loop
(via the `Halted` match arm) and never again inside it — a completion arriving *between* two of
the loop's own raw exits went unserviced until the next periodic tick, or forever if none came
(todo.md §14.2 H9 items 20/21/22's own flagged, previously-unfixed gap). Every existing fixture in
this directory reaches at most **one** real VM exit inside the burst loop before spinning
(`../halt-then-spin-guest`) or times out entirely (`../spin-guest`), so none of them could exercise
a notify-count change *between* two burst-loop exits — there was never a second exit to change
between. This fixture's three separate `out` instructions give the burst loop three real exits in
a row, each one a distinct point at which a fake test device's `notify_count` (tied to the guest's
own growing console output) changes — `burst_loop_services_devices_between_raw_exits`
(`crates/baud-multiverse/src/linux/mod.rs`) asserts the fake device's `service_running` callback
fires exactly three times, once per marker, proving the fix actually detects and services each one
inside the loop rather than only at its next tick boundary.
