<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Tracing Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-tracing` is observation plane 2: an independent, kernel-side witness of the execution the supervisor
claims to have mediated. It exists to cross-check the supervisor's syscall log (plane 1) and to catch
supervisor bugs or escaped guests.

### Goals

- **Independent witness**: observe the same execution by a different mechanism
- **Cross-check**: per-guest syscall counts and sequences must agree with plane 1
- **Graceful degradation**: a `/proc`+strace fallback emits the same schema when BPF is unavailable
- **No workload knowledge**: knows processes and syscalls, never workload semantics

### Non-Goals

- Being the primary observation source (plane 1 is)
- Dynamic BPF compilation inside sandboxes

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                  baud-tracing                   │
│  aya CO-RE probes (prebuilt) → ringbuf         │
│  fallback: /proc sampling + strace shim        │
└──────────────────────────────────────────────┘
        ▲ loaded by baud-tape-agent
```

### Rationale

- Fixed, prebuilt CO-RE probe set — no compilation in-sandbox.
- Events keyed by a `{pid → node-id}` mapping supplied by the agent.

---

## 3. Probe Set

| Event | Source |
| ---------------------- | ------------------------------ |
| sched switch/exec      | tracepoints |
| syscall entry/exit     | raw tracepoints (supervisor + guests) |
| page faults            | tracepoint |

All emit `EbpfRecord { node, event, value, vtime, source }` where `source ∈ {Native, Fallback}`.

---

## 4. Cross-Check

`baud verify observation --run` compares plane 1 (supervisor syscall log) against plane 2 (this crate) for
a run:

- Per-guest syscall counts and sequences must agree.
- Disagreement indicates a supervisor bug or an escaped guest and **fails the run**.

---

## 5. Fallback

If the sandbox kernel denies BPF (likely on shared container runtimes):

- Degrade to `/proc` sampling + a strace shim emitting the **same** `EbpfRecord` schema, flagged
  `source=Fallback`.
- The cross-check still runs against the fallback stream.
- On a host where even that is unavailable but `auditd` is present (a CI host, per `infra/nixos-modules/security-audit.nix`,
  plan §11.3), an `auditd` execve/syscall stream is a coarser, kernel-independent third source that maps onto
  the same schema. It is a reference/backstop, not the primary plane.

---

## 6. Testing

```rust
#[test]
fn planes_agree_on_healthy_run() {
    assert_eq!(syscall_seq(run.syscall_log()), syscall_seq(run.ebpf_stream()));
}

#[test]
fn fallback_emits_same_schema() {
    assert_eq!(fallback_probe().sample().source, Source::Fallback);
}
```

- A deliberately broken supervisor build (test fixture) fails the cross-check.

---

## 7. Capability Considerations

| Concern                | Handling                                    |
| ---------------------- | ------------------------------------------- |
| BPF denied (H0/M7)     | Fallback shim, mandatory                     |
| Kernel version drift   | CO-RE relocations; capabilities probed at boot |
| PID→node mapping       | Supplied by the agent; required for attribution |

---

## 8. Security Considerations

| Threat                        | Handling                                    |
| ----------------------------- | ------------------------------------------- |
| BPF program abuse             | Fixed prebuilt CO-RE set; no in-sandbox compilation |
| Plane-2 tampering hides an escape | Cross-check vs plane 1; disagreement fails the run |
| PID reuse misattribution      | Per-run `{pid → node}` map supplied by the agent |

---

## 9. Future Considerations

| Feature            | Description                                    |
| ------------------ | ---------------------------------------------- |
| Richer probes      | Network/file syscall argument capture          |
| Userspace uprobes  | Per-guest function-level tracing               |
