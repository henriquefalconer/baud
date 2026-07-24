<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Identity Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-identity` mints and verifies workload identities. The server is the sole trust root; each tape (and
each node within it) receives a short-lived signed token it presents on every connection. Nothing is
accepted unauthenticated — sandbox preview URLs are public.

### Goals

- **Server as trust root**: only the server mints tokens
- **Short-lived credentials**: 10-minute TTL, renewed
- **Per-node attribution**: derived identities let observations be attributed to a node
- **No owned cryptography**: signing and JWT handling come from vetted crates
- **Small surface**: signed tokens only, no external infrastructure

### Non-Goals

- x509 certificates, trust-domain federation, or attestation plugins
- Long-lived credentials
- Implementing signing or token formats by hand

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                baud-identity                 │
│  ed25519 keypair (root) → signed tokens        │
│  verify on every agent connection              │
└──────────────────────────────────────────────┘
        ▲ used by baud-server (mint) and baud-tape-agent (present)
```

### Rationale

- Signing via `ed25519-dalek`, JWT encode/verify via `jsonwebtoken`. baud owns the token subject scheme,
  TTL policy, and verification rule — never the cryptography.
- Root key held as `SecretString` (baud-secret); tokens held as `SecretString` in transit.

---

## 3. Identity Scheme

```
baud://tape/<sandbox-id>/run/<run-id>            # a tape
baud://tape/<sandbox-id>/run/<run-id>/node/<i>   # a node within it
```

- Tape token minted at sandbox creation; node identities derived from it.
- TTL 10 minutes; the agent renews before expiry.

---

## 4. Verification

| Point | Rule |
| -------------------- | ------------------------------------------ |
| Agent → server       | Token required on every connection; verified against the root public key |
| Expired/absent token | Connection refused |
| Node attribution     | Observations carry the node identity |

---

## 5. Testing

```rust
#[test]
fn expired_token_is_refused() {
    assert!(verify(&mint(ttl_ago(11.min()))).is_err());
}

#[test]
fn wrong_root_key_is_refused() {
    assert!(verify_with(other_root(), &mint(now())).is_err());
}
```

- A connection without a valid token is refused.
- Observations are attributed to the correct node identity.

---

## 6. Security Considerations

| Threat                        | Handling                                    |
| ----------------------------- | ------------------------------------------- |
| Public preview URL            | Token required; unauthenticated refused     |
| Token leakage                 | Short TTL; held as `SecretString`; never logged |
| Root key compromise           | Root key in baud-keys (age-encrypted at rest) |

---

## 7. Future Considerations

| Feature       | Description                                        |
| ------------- | ------------------------------------------------- |
| Key rotation  | Rotate the root signing key without downtime       |
| mTLS option   | Mutual TLS on the agent channel alongside tokens   |
