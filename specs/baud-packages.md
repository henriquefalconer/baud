<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Packages Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

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
