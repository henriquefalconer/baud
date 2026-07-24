<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Secret Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-secret` provides a type-safe wrapper for sensitive values that prevents accidental exposure through
logging, serialization, or debugging. API keys, tokens, and private keys are never leaked in logs, error
messages, or configuration dumps.

### Goals

- **Type-level protection**: sensitive values are wrapped in a type that enforces redaction at compile time
- **Logging safety**: values never appear in structured or unstructured logs
- **Serialization safety**: values serialize as `[REDACTED]`
- **Memory safety**: values are zeroized from memory on drop
- **Explicit access**: reading the inner value requires an explicit `.expose()` call
- **File-based loading**: support the `*_FILE` convention for mounted secrets

### Non-Goals

- Encrypted at-rest storage (that is `baud-keys`)
- Hardware security module integration
- Constant-time comparison

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                baud-secret                   │
│  - Secret<T> type                              │
│  - SecretString alias                          │
│  - Redacted Debug/Display/Serialize            │
│  - Zeroize on drop                             │
│  - load_secret_env / require_secret_env        │
│  - No config, network, or workload knowledge   │
└──────────────────────────────────────────────┘
                     ▲
                     │ depends on
   baud-keys · baud-tape · baud-identity · baud-tape-agent
```

### Rationale

- A standalone primitive crate with no business logic.
- Domain crates depend on it directly to hold any token as `SecretString` instead of `String`.

---

## 3. Secret<T> Type

### Definition

```rust
#[derive(Zeroize)]
#[zeroize(drop)]
pub struct Secret<T>
where
    T: Zeroize,
{
    inner: T,
}

pub type SecretString = Secret<String>;

pub const REDACTED: &str = "[REDACTED]";
```

### Key Properties

| Property               | Implementation                                     |
| ---------------------- | -------------------------------------------------- |
| **No Deref**           | Must call `.expose()` to read the inner value      |
| **Zeroize on drop**    | Memory is zeroed when the secret is dropped        |
| **Redacted Debug**     | `format!("{:?}", s)` → `Secret("[REDACTED]")`      |
| **Redacted Display**   | `format!("{}", s)` → `[REDACTED]`                  |
| **Redacted Serialize** | JSON/TOML output: `"[REDACTED]"`                    |
| **Normal Deserialize** | Loads the value normally from config               |
| **Clone**              | Clones the inner value (requires `T: Clone`)       |
| **PartialEq/Eq**       | Compares inner values                              |

### API

```rust
impl<T: Zeroize> Secret<T> {
    fn new(inner: T) -> Self;
    fn expose(&self) -> &T;          // visible in code review
    fn expose_mut(&mut self) -> &mut T;
    fn into_inner(self) -> T where T: Clone;
}
```

---

## 4. File-Based Loading

### Precedence

1. If `{VAR}_FILE` is set → read the secret from that file path
2. Else if `{VAR}` is set → use its value directly
3. Else → `None`

### API

```rust
fn load_secret_env(var: &str) -> Result<Option<SecretString>, SecretEnvError>;
fn require_secret_env(var: &str) -> Result<SecretString, SecretEnvError>;
```

### File Format

- A single trailing newline is stripped.
- All other content is preserved as-is.
- Empty files produce empty secrets (may fail downstream validation).

---

## 5. Testing

```rust
proptest! {
    #[test]
    fn debug_never_contains_secret(inner in "[a-zA-Z0-9]{3,50}") {
        let s = Secret::new(inner.clone());
        prop_assert!(!format!("{:?}", s).contains(&inner));
    }

    #[test]
    fn serialize_never_contains_secret(inner in "[a-zA-Z0-9]{3,50}") {
        let s = Secret::new(inner.clone());
        prop_assert!(!serde_json::to_string(&s).unwrap().contains(&inner));
    }
}
```

---

## 6. Security Considerations

### Protects Against

| Threat                          | Protection                      |
| ------------------------------- | ------------------------------- |
| Secrets in logs                 | Debug/Display always redacted   |
| Secrets in error messages       | Debug impl is redacted          |
| Secrets in config dumps         | Serialize always redacted       |
| Secrets in core dumps           | Zeroize on drop clears memory   |
| Accidental string interpolation | No Deref; must call `.expose()` |

### Does NOT Protect Against

| Threat                         | Mitigation                        |
| ------------------------------ | --------------------------------- |
| Deliberate `.expose()` in logs | Code review, linting              |
| Memory inspection before drop  | Use shorter-lived secrets         |
| Secrets in version control     | `.gitignore`, secret scanning     |

---

## 7. Future Considerations

| Feature                 | Description                                  |
| ----------------------- | -------------------------------------------- |
| Keyring integration     | Load secrets from the system keyring         |
| Audit logging           | Log access events (never values)             |
| `secrecy` migration     | `Secret<T>` could become a thin wrapper, non-breaking |
