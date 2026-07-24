<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Tape Local Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-tape-local` implements the `Backend` trait by running the tape-agent as a local subprocess in a
temporary directory. It exists so CI and integration tests run without cloud or cost, and so backend parity
is provable.

### Goals

- **Zero-cost testing**: no cloud, no account, no network
- **Parity**: passes the same conformance suite as baud-tape
- **Simplicity**: a process, a directory, a socket — no sandboxing theater

### Non-Goals

- Real isolation or resource limits matching Daytona
- Determinism guarantees beyond what the supervisor provides (that is baud-multiverse's job)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│              baud-tape-local              │
│  Backend trait over a local subprocess         │
│  temp dir · UDS · fork/exec + pidfd            │
└──────────────────────────────────────────────┘
        ▲ selected by baud-server behind Backend
```

### Rationale

- On Linux, runs natively. On macOS dev machines, runs inside a lima/colima VM (checked by `doctor`), since
  the supervisor needs Linux.

---

## 3. Behavior

| Backend method | Local implementation |
| -------------- | ------------------------------------------ |
| `create`       | Make a temp dir, start the agent process |
| `exec`         | Run argv in the sandbox dir |
| `put`/`get`    | Copy files in/out of the temp dir |
| `status`       | Process liveness (no auto-stop/archive timers; simulated on request for tests) |
| `endpoint`     | Local UDS / loopback port |
| `destroy`      | Kill the process tree, remove the temp dir |

Auto-stop/auto-archive semantics are simulated on demand so lifecycle tests (M1) run without waiting on real
timers.

---

## 4. Testing

```rust
#[test]
fn backend_conformance_parity() {
    for backend in [local(), daytona_fixture()] {
        run_conformance_suite(backend); // identical assertions on both
    }
}
```

- **Shared conformance suite**: one suite runs against both backends; a feature that works on only one fails CI.
- Used as the default backend for H1–H3 and all non-cloud milestone drives.

---

## 5. Parity Considerations

| Concern                     | Rule                                        |
| --------------------------- | ------------------------------------------- |
| Behavioral divergence       | Any observable difference from Daytona is a bug |
| Platform (macOS)            | lima VM required; `doctor` verifies |
| Resource limits             | Best-effort; not a correctness surface |

---

## 6. Security Considerations

| Threat                        | Handling                                    |
| ----------------------------- | ------------------------------------------- |
| Guest escape                  | The supervisor is the boundary, not the backend |
| Stale process/dir leaks       | `destroy` kills the process tree and removes the temp dir |
| macOS host exposure           | Runs inside a lima VM; never on the host kernel |

---

## 7. Future Considerations

| Feature            | Description                                    |
| ------------------ | ---------------------------------------------- |
| cgroup caps        | Mirror Daytona resource quotas                 |
| Parallel local tapes | Multiple concurrent subprocess sandboxes     |
