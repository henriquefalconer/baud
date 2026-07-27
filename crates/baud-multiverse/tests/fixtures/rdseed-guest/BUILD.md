<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# `rdseed-guest` — the enforced-regime RDSEED serve path's bootable fixture

Same rationale and build mechanics as `../hello-guest/BUILD.md` and `../rdrand-guest/BUILD.md`
(hand-assembled flat binary wrapped in a minimal bzImage header
`linux_loader::loader::bzimage::BzImage::load` accepts, no kernel source tree/Nix/cross-compiler
needed) — only the payload, plus one extra build step, differ. Regenerate with `python3 build.py`
(needs only `as`/`ld`).

## What it does

`payload.s` is hand-written x86-64: write one marker byte (`'S'`, 0x53) to COM1 (`out dx, al` at
port `0x3f8`), then execute `rdseed eax` — encoded as raw bytes (`.byte 0x0F, 0xC7, 0xF8`) rather
than as a mnemonic, so no assembler-feature gate can silently change the encoding out from under
the rewrite step — then write the 4 raw result bytes to COM1 one at a time, then `hlt` in a loop.
Byte-for-byte the same shape as `../rdrand-guest/payload.s`, with `'S'` in place of that fixture's
`'X'` marker so the two are never confused in a console transcript.

## The extra build step: `rdseed` → `UD2` + `NOP`, at build time

**The checked-in `bzImage` contains no `rdseed` instruction at all.** `build.py`'s `rewrite_rdseed`
overwrites the 3-byte `0F C7 F8` encoding, in place and length-preserving, with `0F 0B 90` —
`UD2` plus one `NOP` of padding — which is byte-for-byte what
`baud_packages::rewrite_rdseed` (`crates/baud-packages/src/rdseed.rs`, todo.md §4) emits for this
site, and therefore exactly what a real `baud image build` would hand the VMM.

`build.py` does the patch itself rather than shelling out to that crate because this fixture is a
flat binary, not the ELF `rewrite_rdseed` parses (it walks executable sections of an ELF). The
*bytes written* are identical either way, and the bytes are the only thing the enforced-regime
serve path can observe. `build.py` refuses to proceed if `payload.s` stops containing exactly one
`rdseed eax` encoding, so the fixture can never silently drift out of sync with the address the
Rust test hardcodes below.

This is why this fixture needs no `SECONDARY_EXEC_RDSEED_EXITING`: that VMX secondary control is
**not settable on this dev host's microcode** (`kernel-module/baud-enforced/BUILD.md`'s probe
report), but the real `RDSEED` opcode never executes in the guest, so the control is moot for this
path. The `UD2` traps instead, via the exception bitmap stock KVM already sets `UD_VECTOR` in —
see `kernel-module/baud-enforced/ud2-enforce.patch`.

## Where the UD2 is (the numbers the Rust test hardcodes)

`baud-server`'s `rdseed_sites` module now wires a real `RdseedRewriteReport` sidecar into
`WorkClock::with_rdseed_sites` for any real `/run/kvm*` boot (todo.md §14). That pipeline reads an
ELF's `SHF_EXECINSTR` sections (`baud_packages::rewrite_rdseed`) — this fixture is a hand-assembled
**flat binary**, never an ELF, so it never goes through that pass and has no sidecar of its own.
Like `rdtsc-guest`/`rdrand-guest`, this is a fixed, hand-verified binary, so the site table stays
hardcoded in the test from the values `build.py` prints on every regeneration:

| | value | derivation |
|---|---|---|
| payload offset of `UD2` | `0x07` | 4-byte `mov dx,0x3f8` + 2-byte `mov al,0x53` + 1-byte `out dx,al` |
| `bzImage` file offset | `0xC07` | `SETUP_SIZE` (2560 = `0xA00`) + `ENTRY_OFFSET` (`0x200`) + `0x07` |
| **guest address of `UD2`** | **`0x0020_0207`** | `layout::KERNEL_LOAD_ADDR` (`0x20_0000`) + `layout::KERNEL_64BIT_ENTRY_OFFSET` (`0x200`) + `0x07` |
| `EnforcedRdseedSite::gpr_index` | `0` | `RAX`/`EAX` — ModRM `reg` field of `0F C7 F8` is `/7` with `rm = 000` = eax; `0` in `write_enforced_rdseed_result`'s 0=RAX..15=R15 numbering |
| `EnforcedRdseedSite::length` | `3` | the original `RDSEED r32` encoding's length, so RIP resumes at `0x0020_020A` (`mov ecx,4`), past the `NOP` padding, not at it |

The guest address is exact, not approximate: `BzImage::load` honours the `kernel_offset` baud
passes it verbatim (`kernel_load == KERNEL_LOAD_ADDR`, `linux-loader-0.14.0/src/loader/bzimage/
mod.rs`), skips exactly `SETUP_SIZE` bytes of setup sectors, and `boot_guest` then sets
`RIP = kernel_load + KERNEL_64BIT_ENTRY_OFFSET`. Verified by disassembling the checked-in image:

```
$ objdump -D -b binary -m i386:x86-64 -M intel --adjust-vma=0x200200 <body of bzImage>
  200200:  66 ba f8 03      mov    dx,0x3f8
  200204:  b0 53            mov    al,0x53
  200206:  ee               out    dx,al
  200207:  0f 0b            ud2            <-- the rewritten rdseed site
  200209:  90               nop
  20020a:  b9 04 00 00 00   mov    ecx,0x4 <-- where RIP resumes after a served value
```

## What each regime does with it

- **Cooperative (stock `kvm_intel.ko`, a normal `cargo test --workspace`)**: the `UD2` traps to
  stock `handle_ud`, which fails to emulate it and injects `#UD`. This fixture has no IDT, so that
  cascades to a triple fault, which `baud-vcpu`'s run loop treats identically to a clean `Hlt`
  (`VcpuExit::Shutdown` -> `DispatchOutcome::Halted`) — same mechanism `../rdrand-guest/BUILD.md`
  documents for its own `#UD`. Console output is the single marker byte `S`.
- **Enforced (patched module, site registered)**: `handle_baud_ud2_exit` recognizes the `UD2` and
  hands it to userspace as `KVM_EXIT_BAUD_DETERMINISM` (payload low byte 2) with RIP left *at* the
  `UD2`; `dispatch_exit` resolves `0x0020_0207` through the registered site table, serves
  `WorkClock::serve_enforced_rdseed()` into `EAX`, sets `RFLAGS.CF`, and advances RIP by 3. The
  guest reaches the echo loop: `S` plus 4 value bytes, identical across two boots of the same tape.
- **Enforced (patched module, site *not* registered)**: `resolve_rdseed_site` returns `None`,
  `DispatchOutcome::ReinjectUd` re-injects `#UD` at the untouched RIP, and the guest ends up
  exactly where the cooperative regime leaves it — output `S`, no bogus value served. This is the
  same path a real kernel `BUG()`/`WARN_ON()` (also a bare `UD2`) and any genuinely invalid opcode
  take, and `ud2_outside_the_rdseed_site_table_reinjects_ud` asserts it directly.

Both enforced-regime tests live in `crates/baud-multiverse/src/linux/mod.rs`
(`rdseed_enforced_regime_is_bit_exact_across_boots`,
`ud2_outside_the_rdseed_site_table_reinjects_ud`), `#[ignore]`d so a normal `cargo test
--workspace` against the stock module never runs them; `drive/manual/h3-enforced-rdseed.sh` invokes them
by name after swapping the patched module in.
