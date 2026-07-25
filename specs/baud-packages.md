<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Packages Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

> **2026-07-24 KVM pivot notice.** §§1-8 below describe this crate's original scope: building a
> single **static, no-PIE, musl ELF process** for a ptrace-based tracee. Under the
> deterministic-hypervisor pivot (`todo.md`, top-level plan), a workload is no longer a lone
> process traced from userspace — it is a **bootable guest image** (kernel + rootfs + a tiny
> in-guest agent) that `baud-multiverse` boots and owns at the KVM/VT-x layer (`todo.md` §4). The
> static/no-PIE/musl constraint was a **ptrace-tracee** limitation, not a hardware one; the pivot's
> whole point was removing it — a guest under KVM may run threads, dynamic binaries, multiple
> processes, any language (`todo.md` §0, §4). §§1-8's flake-templating machinery (`spec.toml` →
> pinned Nix flake → `nix build`/`nix copy` → closure hash) is **still valid and still used**, just
> demoted from "the top-level deliverable's contract" to "how you build one piece that ends up
> inside a guest image's rootfs" (e.g. the in-guest agent binary itself, or a workload binary the
> guest's init runs) — `BuildResult::verify_guest_contract`'s static/no-PIE check is retained
> unchanged for exactly that narrower use.
>
> **§9 below is the new top-level contract**: what a guest image's kernel must and must not do to
> satisfy `baud-multiverse`'s determinism guarantees, and `baud image lint` — the command that
> enforces it (`todo.md` §4's `image_lint_requires_tape_driver` test, problem/spec/test matrix
> row 14). Read §9 first; treat §§1-8 as "how workload pieces get built," not "the guest contract."

---

## 1. Overview

### Purpose

`baud-packages` builds guest binaries and fixtures from pinned Nix building blocks. A workload's TOML spec
generates a pinned flake, whose closure produces static, no-PIE, musl guests that satisfy the supervisor's
contract. The closure hash goes into the run manifest as environmental determinism.

### Goals

- **Reproducible builds**: pinned nixpkgs revision; identical inputs → identical closure
- **Contract-compatible guests**: static, no-PIE, musl by construction
- **Environmental determinism**: closure hash recorded and verified
- **Simple templating**: substitution into one flake template, no Nix-language metaprogramming

### Non-Goals

- Wrapping Nix features beyond `build` and `copy`
- Nix-language AST manipulation

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                  baud-packages                    │
│  spec.toml → pinned flake → nix build/copy     │
│  closure hash → manifest                       │
└──────────────────────────────────────────────┘
        ▲ invoked by baud-tape-agent
```

### Rationale

- One flake template + substitution; the pinned nixpkgs rev lives in exactly one place.
- Any nixpkgs-expressible derivation that satisfies the guest contract is a valid workload.

---

## 3. Spec → Flake

```toml
# spec.toml (abbreviated)
[workload]
name = "parser"
packages = ["stdenv", "musl"]
build = "cc -static -no-pie -o guest parser.c"
```

Generates a flake pinned to a fixed nixpkgs rev; `nix build` yields the guest; `nix copy` warms the store.

---

## 4. Economics & the Sandbox Image

- To fit the 1-minute sandbox auto-stop, closures are restored from a **prebaked Daytona snapshot image**
  with a warm `/nix/store`; cold builds are the exception, not the rule.
- That image is built reproducibly by `infra/pkgs/baud-sandbox-image.nix` (`dockerTools.buildImage`; plan
  §11.2), baking the agent, supervisor, tracing probes, and a warm store. Its closure hash is journaled and
  feeds this crate's environmental-determinism story.
- The agent and supervisor binaries themselves are cross-built (macOS host → static musl linux) by the
  `infra/pkgs` fenix overlay — the same overlay this crate's guest builds share a pinned nixpkgs rev with.
- Closure hash journaled per run; reconstruction requires the same closure.

---

## 5. Testing

```rust
#[test]
fn build_is_reproducible() {
    assert_eq!(build(spec).closure_hash, build(spec).closure_hash);
}

#[test]
fn guest_is_static_no_pie() {
    let elf = build(spec).guest;
    assert!(elf.is_static() && !elf.is_pie());
}
```

- 1 GiB disk: the store required by `parser`/`framedemo`/`raftlet` fits or the snapshot image is used.

---

## 6. Risk Considerations

| Risk                        | Handling                                    |
| --------------------------- | ------------------------------------------- |
| 1 GiB disk too small        | Prebaked warm-store snapshot image          |
| Guest not contract-compatible | Static/no-PIE scan fails `spec lint`      |
| nixpkgs rev drift           | Pinned in one place; changing it is a deliberate, reviewed edit |

---

## 7. Security Considerations

| Threat                        | Handling                                    |
| ----------------------------- | ------------------------------------------- |
| Supply-chain drift            | Pinned nixpkgs rev; closure hash in the manifest |
| Non-contract guest slips through | Static/no-PIE scan at `spec lint` rejects it |
| Build reaches the network     | Nix build sandbox; fixed inputs only        |

---

## 8. Future Considerations

| Feature            | Description                                    |
| ------------------ | ---------------------------------------------- |
| Remote binary cache | Shared warm store across sandboxes            |
| Closure signing    | Verify closure provenance before a run         |

---

## 9. The Guest Image Contract (KVM pivot, `todo.md` §4)

### 9.1 What a guest image must satisfy

A workload is a **bootable guest image**: a Linux kernel (or unikernel) + a rootfs with the
software under test + a tiny in-guest agent that speaks to the tape device. Before
`baud-multiverse` will boot it under a determinism guarantee, the guest kernel's build must:

1. **Include the tape-device driver** — the boot-time kernel shim that talks to
   `baud-tape-device`'s PIO/MMIO window (`specs/baud-tape-device.md` §2's "guest-side driver
   contract"). Without it the guest has no way to take entropy, clock, or external input from the
   tape at all — every read that should be tape-derived instead falls through to whatever the
   kernel's stock drivers do, which is exactly the determinism hole the whole plan exists to close.
2. **Not enable a real hardware timer `baud-multiverse` does not model.** `baud-multiverse`'s
   device bus (`crates/baud-multiverse/src/console.rs`'s `DeviceBus`) serves exactly two things:
   the console and the tape device; every other port/MMIO address falls through to a fixed-byte
   open-bus fallback (`specs/baud-multiverse.md` §4/§6, `todo.md` §3.6's "down to a console plus
   the tape device"). A guest kernel built with the RTC or HPET enabled either hangs waiting on a
   device that never answers in the way it expects, or — the failure mode that actually matters —
   reads real host time through a path the VMM never intended to expose, silently reintroducing the
   nondeterminism the tape/work-clock model exists to remove (`todo.md` §3.3, §3.6).

### 9.2 `baud image lint`

`baud-packages`'s `image` module (`crates/baud-packages/src/image.rs`) implements this contract as
a lint over a Linux kernel `.config` — the standard Kconfig-output text format nix's kernel builder
(and `make menuconfig`) both produce, so this reads the artifact the build already has, no new
build-time instrumentation required:

- `GuestImageManifest::parse_kernel_config(text)` parses `CONFIG_FOO=y`/`=m` assignments and
  `# CONFIG_FOO is not set` disables into a symbol → `ConfigState` map (`Yes`/`Module`/`No`); an
  unmentioned symbol defaults to `No`, matching Kconfig's own convention.
- `image_lint(manifest)` checks two things, both reported together (a caller should not have to
  fix-and-relint twice to see every violation):
  1. `CONFIG_BAUD_TAPE_DEVICE` (the Kconfig symbol baud's out-of-tree tape-device shim registers
     under) must be `Yes` or `Module`.
  2. None of `CONFIG_RTC_CLASS`, `CONFIG_RTC_DRV_CMOS`, `CONFIG_HPET_TIMER`, `CONFIG_HPET_MMAP`
     (§3.3's "delete HPET/RTC entirely," `FORBIDDEN_REAL_TIMERS`) may be `Yes` or `Module`.
- Each violation carries a `symbol` and a human-readable `reason` — `baud image lint` fails with a
  specific, actionable reason per violation, never a bare "invalid."
- Wired end-to-end: `baud_packages::lint_kernel_config` (crate) → `POST /image/lint`
  (`baud-server`) → `baud image lint <path>` (`baud-cli`, exits `1` on any violation — never a
  false pass, mirroring `baud host probe`'s rejected-regime handling).

### 9.3 Test

- **`image_lint_requires_tape_driver`** (`todo.md` §4, problem/spec/test matrix row 14): an image
  `.config` without the tape-device driver, or with a real RTC/HPET enabled, fails `baud image
  lint` with a specific reason naming the offending symbol. (`crates/baud-packages/src/image.rs`'s
  test module; plus `image_lint_rejects_real_rtc`/`_real_hpet`, `well_formed_image_passes_lint`,
  and a property test asserting every subset of the forbidden-timer set is fully reported
  regardless of which symbols are enabled or what order the `.config` lists them in.)

### 9.4 Not yet built

- `PIT`/`PM-timer` are named alongside RTC/HPET in `todo.md` §3.3's "delete entirely" but have no
  single canonical boolean Kconfig symbol as clean as HPET/RTC's (PIT is usually compiled in as
  part of the core x86 platform code, not a separately toggleable driver) — tracked as a follow-up
  once a real guest kernel `.config` is available to check what actually needs gating.
- **The real guest-image pipeline is specified in `todo.md` §4 and consumed by `specs/baud-boot.md`**:
  a minimal builtin kernel + reproducible initramfs (`Buildroot qemu_x86_64_defconfig` → pinned Nix
  `linux_6_12.override` + `makeInitrdNG`), a static `/init`, and the harness
  (`specs/baud-guest-harness.md`), with image identity = `sha256(bzImage ‖ initramfs.gz)`. This
  replaces the old single-static-musl-binary output. `lint_kernel_config` today operates on a
  `.config` text handed to it; the build side that produces that `.config` plus the two artifacts from
  a `spec.toml` is the next step (`todo.md` §14 next-action 1). `baud-boot` then writes the boot_params
  / E820 / `SETUP_RNG_SEED` / deterministic cmdline for whatever image this pipeline emits.
