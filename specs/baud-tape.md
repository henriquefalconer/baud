<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Tape Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-tape` is the Daytona-backed implementation of the `Backend` trait: it creates, execs, files, and
tears down the sandboxes that host baud-tapes, enforcing the resource and lifecycle parameters baud
depends on.

### Goals

- **Trait conformance**: satisfies the same `Backend` trait as the local backend
- **Parameter enforcement**: 1 vCPU / 1 GiB / 1 GiB, auto-stop 1m, auto-archive 5m
- **Only what is used**: wraps only the endpoints baud calls, enumerated in the crate README
- **Resilience**: retries with backoff; isolates API drift

### Non-Goals

- A general Daytona SDK
- Any logic above the `Backend` trait

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                baud-tape                  │
│  typed REST client → Daytona API               │
│  implements Backend trait                      │
└──────────────────────────────────────────────┘
        ▲ selected by baud-server behind Backend
```

### Rationale

- Hidden behind the `Backend` trait; nothing above the trait imports this crate.
- API key held as `SecretString` (baud-secret); never logged.

---

## 3. Backend Trait

```rust
trait Backend {
    fn create(&self, spec: SandboxSpec) -> Result<TapeId>;
    fn destroy(&self, id: &TapeId) -> Result<()>;
    fn exec(&self, id: &TapeId, argv: &[String]) -> Result<ExecOut>;
    fn put(&self, id: &TapeId, path: &str, bytes: &[u8]) -> Result<()>;
    fn get(&self, id: &TapeId, path: &str) -> Result<Vec<u8>>;
    fn status(&self, id: &TapeId) -> Result<TapeStatus>;   // Running|Stopped|Archived|Gone
    fn endpoint(&self, id: &TapeId, port: u16) -> Result<Url>; // preview URL
}
```

---

## 4. Enforced Parameters

| Parameter        | Value | Notes |
| ---------------- | ----- | ------------------------------------------ |
| vCPU             | 1     | |
| RAM              | 1 GiB | |
| Disk             | 1 GiB | if rejected, use platform minimum, record actual in manifest |
| Auto-stop        | 1 min | design forcing function |
| Auto-archive     | 5 min | |

---

## 5. Wrapped Endpoints

Only: create / start / stop / archive / delete sandbox, exec, file upload, file download, preview URL. Any
endpoint not on this list is out of scope until a milestone needs it.

---

## 6. Testing

```rust
#[test]
fn enforces_sandbox_shape() {
    let s = client.build_spec();
    assert_eq!((s.vcpu, s.ram_gib, s.autostop_s), (1, 1, 60));
}
// contract tests replay recorded request/response fixtures — no live API in CI
```

- Backend conformance suite (shared with the local backend) passes identically.
- Lifecycle: `create → status Running → (wait) Stopped → ensure → Archived → ensure → Gone`.

---

## 7. Risk & Fallback Considerations

| Risk                          | Handling                                    |
| ----------------------------- | ------------------------------------------- |
| API drift                     | Isolated here; fixtures catch changes       |
| Preview-URL/WS blocked        | Agent transport falls back to exec+file polling |
| Disk minimum > 1 GiB          | Accept minimum; record deviation in manifest |

---

## 8. Security Considerations

| Threat                          | Handling                                    |
| ------------------------------- | ------------------------------------------- |
| API key exposure                | Held as `SecretString` (baud-secret); never logged |
| Public preview URL              | Agent connection requires an identity token |
| Untrusted API responses         | Typed client; unexpected shapes are errors, not executed |

---

## 9. Future Considerations

| Feature            | Description                                    |
| ------------------ | ---------------------------------------------- |
| Sandbox pooling    | Pre-warmed sandboxes to beat the 1-minute economics |
| CPU-class pinning  | Pin CPU class for reconstruction determinism   |
