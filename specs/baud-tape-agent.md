<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Tape Agent Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-tape-agent` is the in-sandbox process. It provisions the workload, launches the supervisor with the
guest set, relays protocol draws between the server and the supervisor, applies input and probe adapters,
and streams observations out. It contains no workload logic.

### Goals

- **Faithful relay**: connect the server's driver to the supervisor's device models
- **Adapter execution**: apply the spec's declared input/probe/display adapters
- **Survivability**: stream durably so a killed sandbox loses nothing un-acked
- **Zero workload knowledge**: a new workload kind requires no agent change

### Non-Goals

- Deciding draws (that is the driver) or mediating syscalls (that is the supervisor)
- Any workload-specific parsing beyond the closed adapter set

---

## 2. Crate Architecture

```
┌───────────────────────────────────────────────────────────┐
│                    baud-tape-agent                        │
│  static musl x86_64-linux binary                            │
│  baud-init → baud-packages build → launch baud-multiverse  │
│  relay draws ↔ server · apply adapters · load baud-tracing   │
└───────────────────────────────────────────────────────────┘
        ▲ started by a Backend inside the sandbox
```

### Rationale

- Cross-built (macOS host → static musl x86_64 linux) by the `infra/pkgs` fenix overlay (plan §11.2), and
  baked into the sandbox image alongside the supervisor and tracing probes.
- Binary size budget ≤ 10 MiB; no shell-outs except `nix build`; children via `fork/exec` + pidfd.

---

## 3. Responsibilities

| Step | Action |
| ------------- | ------------------------------------------------ |
| Provision     | Run baud-init directives; build guests via baud-packages |
| Launch        | Start baud-multiverse with the guest images |
| Relay         | Forward `DrawRequest`/`DrawResult` between server and supervisor |
| Observe       | Apply probe adapters; batch `Observe`/`Syscall`/`Frame` records |
| Stream        | Send batches out (WebSocket over preview URL; fallback exec+file polling) |
| eBPF          | Load baud-tracing probes; forward `EbpfRecord`s |

---

## 4. Transport

- Primary: WebSocket over the sandbox's preview URL, authenticated with the tape's identity token
  (held as `SecretString`).
- Fallback: CBOR batch files pulled via the Backend's exec/file API when WS is unreachable — identical
  payloads.

---

## 5. Testing

```rust
#[test]
fn unmodified_agent_runs_a_new_workload() {
    let agent = build_agent(); // the binary built at M2
    assert!(agent.run(mario_spec).is_ok()); // a new workload requires zero agent change
}
```

- Kill/reconstruct: killing the sandbox mid-run loses no server-acked step.
- Fallback: with WS blocked, the run completes via polling with identical journals.

---

## 6. Security Considerations

| Concern              | Handling                                    |
| -------------------- | ------------------------------------------- |
| Token exposure       | Identity token held as `SecretString`; never logged |
| Untrusted preview URL| Server verifies the token on connect        |
| Workload escape      | Supervisor mediates; agent never grants raw host access |

---

## 7. Future Considerations

| Feature             | Description                                    |
| ------------------- | ---------------------------------------------- |
| Delta batching      | Compress observation batches before upload     |
| Resumable transport | Reconnect and continue a stream without a full replay |
