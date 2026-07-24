<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Init Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-init` is declarative first-boot provisioning for a tape. A YAML user-data document maps to a fixed
set of provision steps and a closed set of adapters. It is the only way workloads get onto a tape, and the
closed adapter set is what keeps workload logic out of baud crates.

### Goals

- **Declarative**: a workload is data, not code
- **Idempotent**: re-running yields the same state
- **Closed surface**: five directive kinds; a closed adapter menu
- **Strict**: unknown directives are hard errors

### Non-Goals

- A general configuration-management system
- Arbitrary scripting hooks

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                 baud-init                    │
│  YAML user-data → provision steps + adapters   │
└──────────────────────────────────────────────┘
        ▲ executed by baud-tape-agent
```

### Rationale

- Adapters are the only extension point; the menu grows only by a schema-reviewed entry.

---

## 3. Directives

Exactly five kinds. Unknown directives are hard errors.

| Directive | Meaning |
| ---------- | ------------------------------------------ |
| `nix`      | Flake ref for the guest closure |
| `files`    | Fixtures to write (bridge scripts, ROMs by path, configs) |
| `env`      | Fixed environment for guests |
| `nodes`    | Topology: name, argv, adapter bindings |
| `adapters` | Bindings from the closed adapter set |

---

## 4. Adapters (closed set)

### Input Adapters

| Adapter | Behavior |
| -------------- | ------------------------------------------ |
| `stdin`        | Tape-derived bytes to the guest's stdin |
| `fifo{path}`   | Tape-derived bytes to a named pipe the guest reads |
| `net`          | Messages via the supervisor's net device |

### Probe Adapters

| Adapter | Behavior |
| ------------------------------- | ------------------------------------------ |
| `stdout-kv{prefix?}`            | Parse `key=value` lines from guest stdout |
| `vfs-file{path, mode}`          | Read a virtual-fs file (`hash`\|`u64`\|`utf8`) |
| `syscall-counter{sysno\|pattern}` | Count matching syscalls from plane 1 |
| `ebpf-counter{event}`           | Count kernel events from plane 2 |
| `exit-hash`                     | Final-state hash from the exit device |

### Display Adapters

| Adapter | Behavior |
| ---------------------------------------- | ------------------------------------------ |
| `frame{width,height,format,transport}`   | Raw frame buffers to baud-stream; format ∈ `rgba8888\|rgb565\|indexed8`, transport ∈ `fifo\|vfs` |

---

## 5. Example (raftlet, abbreviated)

```yaml
nix: "./flake.nix#raftlet"
env: { RUST_BACKTRACE: "0" }
nodes:
  - { name: n0, argv: ["raftlet","--id","0"], adapters: { input: net, probes: [stdout-kv] } }
  - { name: n1, argv: ["raftlet","--id","1"], adapters: { input: net, probes: [stdout-kv] } }
  - { name: n2, argv: ["raftlet","--id","2"], adapters: { input: net, probes: [stdout-kv] } }
```

---

## 6. Testing

```rust
#[test]
fn unknown_directive_is_hard_error() {
    assert!(lint(yaml("bogus: 1")).is_err());
}

#[test]
fn closed_adapter_set_only() {
    assert!(lint(with_adapter("exec-hook")).is_err());
}
```

- Idempotence: provisioning twice on a fresh tape yields identical state.
- Every example workload (`parser`, `framedemo`, `raftlet`, `mario`) is expressible with the closed set.

---

## 7. Extension Considerations

| Change            | Rule                                             |
| ----------------- | ------------------------------------------------ |
| New adapter       | Requires a strict schema and review; no ad-hoc keys |
| New directive     | Strongly discouraged; five is the target surface |

---

## 8. Security Considerations

| Threat                        | Handling                                    |
| ----------------------------- | ------------------------------------------- |
| Directive injection via user-data | Closed five-directive schema; unknown directives are hard errors |
| Adapter abuse                 | Closed, schema'd adapter set; no arbitrary hooks |
| Fixture path escape           | Fixtures written only under the sandbox workdir |

---

## 9. Future Considerations

| Feature            | Description                                    |
| ------------------ | ---------------------------------------------- |
| Adapter versioning | Version the adapter schemas independently      |
| Spec templating    | Parameterized specs for families of workloads  |
