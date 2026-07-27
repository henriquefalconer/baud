<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `idle-halt-guest` — proves `run_until_console_pattern_with_periodic_timer` in isolation

Same rationale and build mechanics as `../timer-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header, no kernel source tree/Nix/cross-compiler needed) — only the
payload differs. Regenerate with `python3 build.py` (needs only `as`/`ld`/`objcopy`/`nm`).

## Why this fixture had to exist

H9's real-Ubuntu-boot attempt (todo.md §14 item 12) reached `Freeing unused kernel memory` — the
very end of kernel init — and then stopped there because every existing `run_to_first_halt_with_*`
combinator treats the guest's first `Hlt` as terminal. A real kernel's idle loop calls `hlt`
(`safe_halt()`, i.e. with `RFLAGS.IF=1`) the instant nothing is runnable and relies on the *next*
periodic timer interrupt alone to reschedule and make progress — indistinguishable, at the level
of "the vCPU is sitting at a halted exit", from a guest that powered off for good. Proving the new
`Multiverse::run_until_console_pattern_with_periodic_timer` primitive actually resumes across
repeated idle halts (rather than terminating at the first one, or hanging) needs a minimal guest
that halts more than once before it does anything else observable — booting the real ~2.2 GiB
Ubuntu image for every test run of this would be far too slow, and would tie the primitive's own
unit test to a huge external asset this repo does not commit.

## What it does

`payload.s` builds a real 64-bit IDT (mirroring `../timer-guest/payload.s` exactly: one interrupt
gate at vector `0x30`, every other vector left not-present), points `IDTR` at it, enables
interrupts (`sti`), then immediately `hlt`s in an infinite `hlt; jmp loop` — no busy loop at all,
unlike `timer-guest`. This models "genuinely idle from the very first instant", the shape that
breaks every existing combinator: `boundary::inject_at`'s arm-early-then-single-step engine cannot
deliver an interrupt to a vCPU that is *already* halted at entry — it reports `Halted` immediately
without ever calling `PmuStepper::inject` — so the only way to wake this guest at all is the
"stage the interrupt directly via `KVM_SET_VCPU_EVENTS`, then let it run natively" idiom
`service_virtio_rng_interrupt_while_halted` established for a single device, generalized here to
the timer channel itself.

The injected vector's handler counts how many times it has been delivered
(`WAKES_BEFORE_MESSAGE = 5`, in `wake_count`). The first four wakes are silent — models a real
guest halting repeatedly for unrelated reasons (blocked on disk I/O, waiting for the next
scheduler quantum) before the text a caller is actually watching for ever appears. Only on the
fifth wake does the handler write `"ubuntu login:"` to COM1 — one byte per `out`, each of which
forces its own VM exit, so a caller resuming past a halt must keep draining exits after delivering
the interrupt rather than assuming a single `step_exit` call is enough in general (unlike
`service_virtio_rng_interrupt_while_halted`'s specific case, where the virtio-rng guest's own ISR
happens to cause no further forced exit before its next halt). After the message, the handler
`iretq`s back into the same `hlt; jmp loop`, so the guest keeps halting forever — irrelevant to
the primitive under test, which returns the moment the pattern appears in the console stream
regardless of what the guest does afterwards.
