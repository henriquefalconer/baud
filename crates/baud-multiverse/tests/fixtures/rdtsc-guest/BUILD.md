<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `rdtsc-guest` — H3's real bootable fixture for the RDTSC-compliance half of "randomness + time control"

Same rationale and build mechanics as `../rdrand-guest/BUILD.md` (hand-assembled flat binary
wrapped in a minimal bzImage header, no kernel source tree/Nix/cross-compiler needed) — only the
payload differs. Regenerate with `python3 build.py` (needs only `as`/`ld`).

## What it does

`payload.s` is hand-written x86-64: write one marker byte (`'T'`) to COM1 (`out dx, al` at port
`0x3f8`), then execute the raw `rdtsc` instruction directly (`edx:eax`), pack the two 32-bit halves
into one 64-bit value (`shl rdx, 32; or rax, rdx`), then write all 8 result bytes to COM1
low-byte-first, then `hlt` in a loop.

## Why this fixture exists

todo.md §3.3 specifies two determinism mechanisms for the raw timestamp instruction under the
cooperative regime: `KVM_SET_TSC_KHZ` pins a fixed frequency (already wired,
`linux::VIRTUAL_TSC_KHZ`), and `KVM_VCPU_TSC_OFFSET` pins the starting value (until this fixture,
*not* wired — `boot_guest` called `set_tsc_khz` but never anchored the actual TSC value, so a raw
`rdtsc` read reflected implicit host-wall-clock-derived state, diverging by however much real time
elapsed between two separate boots, not just scheduling jitter). Unlike `rdrand`/`rdseed`, `rdtsc`
has no CPUID feature gate — masking a CPUID bit cannot hardware-block it the way `rdrand-guest`
found `rdrand` is blocked (see that fixture's `BUILD.md`); a *compliant* guest reading the raw
timestamp still needs the VMM to serve it a reproducible value, which is what
`linux::pin_tsc_value` (called from `boot_guest` right after `set_tsc_khz`, via
`KVM_SET_MSRS(IA32_TSC=0)`) now provides. This fixture is the compliant-guest counterpart to
`rdrand-guest`'s adversarial one: it does exactly what a real guest kernel's TSC calibration path
does (read the raw counter directly), and is the first thing in this workspace to boot twice and
assert that reading actually reproduces.

## Bit-exactness expectation: high bits only, by design

`KVM_SET_MSRS(IA32_TSC=0)` anchors the counter's *starting value* at the moment that ioctl runs
(early in `boot_guest`, before page-table writes and kernel-image loading — both I/O-bound and
themselves a source of run-to-run jitter). Real host-scheduling jitter between that ioctl and the
guest's first `rdtsc` (a handful of microseconds, scaled down by `VIRTUAL_TSC_KHZ`'s ratio to the
host's native rate) still perturbs the *low* bits of the value two boots read. todo.md §3.3's own
test spec anticipates exactly this: "cooperative asserts the high bits / work-derived field" (as
opposed to enforced regime's bit-exact guarantee, which needs the not-yet-built custom KVM module
to force RDTSC-exiting and serve the software work-clock value instead of real elapsed time).
`linux::tests::rdtsc_guest_reproduces_high_bits_across_boots` masks off the low 20 bits
(`RDTSC_JITTER_MASK`) before comparing two boots' values — chosen generously relative to the
few-hundred-cycle jitter actually observed in manual runs on this project's dev host (CLAUDE.md),
while still proving the pin closed the original gap (an *unpinned* raw `rdtsc` would diverge in the
high 44 bits too, by however many host-TSC ticks separate the two boots' wall-clock start times,
typically billions of counts at native GHz frequencies — utterly unrelated to a 20-bit jitter
tolerance).
