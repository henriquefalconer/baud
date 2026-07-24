<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Keys Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-keys` stores provider credentials encrypted at rest and hands them to the rest of the system as
type-safe secrets. It holds the Daytona API key and the identity root key. It owns no cryptography: it drives
the installed `sops` and `age` binaries. Secrets never land on sandboxes — tapes only ever see minted
identity tokens.

### Goals

- **Encrypted at rest**: an age-encrypted file, decryptable only with the local private key
- **No owned cryptography**: encryption/decryption is delegated to `sops` + `age`
- **Type-safe delivery**: decoded values are `SecretString`, never bare strings
- **OS-correct key discovery**: resolve the age key from the right per-OS default
- **Redacted display**: `show --redacted` never prints values

### Non-Goals

- Implementing encryption (delegated to `sops`/`age`)
- KMS/cloud/PGP backends
- In-memory secret typing (that is baud-secret)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                  baud-keys                   │
│  drives `sops`/`age` ──▶ SecretString          │
└───────────────────┬───────────────────────────┘
                    │ depends on
              baud-secret  (Secret<T>, load_secret_env)
```

### Rationale

- Shells out to `sops` and `age`; no cryptographic code lives here. Existing sops-encrypted files and age
  keys work unchanged.
- Depends on baud-secret so every decoded value is wrapped, redacted, and zeroized.

---

## 3. File & Keys

Secrets live under `infra/secrets/` as **per-environment, multi-recipient** sops files (see plan §11.1). Each
file is encrypted to the union of the relevant age recipients (developer keys + a CI key), so several people
and CI can each decrypt without sharing one private key, with least privilege between environments.

```yaml
# infra/secrets/dev.yaml (decrypted view)
daytona:
  api_key: "..."
  api_url: "https://app.daytona.io/api"
github: { token: "..." }
cachix: { cache_name: "...", auth_token: "..." }
identity: { root_key: "..." }        # minted locally by `init`, never from a provider
```

```yaml
# infra/secrets/.sops.yaml — who can decrypt what
keys:
  - &dev_you   age1...
  - &ci_runner age1...                # CI host key via ssh-to-age
creation_rules:
  - path_regex: dev\.yaml$
    key_groups: [ age: [ *dev_you ] ]
  - path_regex: ci\.yaml$
    key_groups: [ age: [ *dev_you, *ci_runner ] ]
```

Encrypted `*.yaml` are committed (ciphertext); age private keys and any decrypted material never are.
Secrets never reach a sandbox — tapes see only minted identity tokens. The SSH-host-key→age path is for
trusted baud-server/CI hosts only, never for Daytona guests.

### Environment Selection & Key Discovery

- `--env <name>` selects the file (`dev.yaml` default; `ci.yaml` on CI). 
- Age private key resolution order:
  1. `SOPS_AGE_KEY_FILE`
  2. macOS default: `~/Library/Application Support/sops/age/keys.txt`
  3. Linux default: `~/.config/sops/age/keys.txt`

`doctor` checks that `sops`, `age`, and `ssh-to-age` are installed, the OS-correct key path exists, and the
current key is a recipient of the selected environment file.

---

## 4. Commands

Each maps to the corresponding `sops` operation; `edit` is the `update-secret` helper (§11.1):

```
baud keys init  [--env dev]   # register the age key, scaffold the encrypted file + .sops.yaml
baud keys edit  [--env dev]   # decrypt → $EDITOR → re-encrypt → verify
baud keys show  --redacted [--env dev]  # print keys with values as [REDACTED]
baud keys rotate [--env dev]  # sops rotate to new recipients
```

---

## 5. Delivery

- The server reads secrets through this crate and receives `SecretString` values (via baud-secret).
- The identity root key lives here (age-encrypted); it is minted locally by `init`, not obtained from any
  provider.

---

## 6. Testing

```rust
#[test]
fn show_redacted_hides_value() {
    let out = keys_show_redacted();
    assert!(out.contains("[REDACTED]") && !out.contains(real_api_key()));
}

#[test]
fn rotate_invalidates_old_key() {
    let f = encrypt_to(key_a());
    rotate_to(key_b());
    assert!(decrypt_with(key_a(), &f).is_err()); // verified against real sops
}
```

- Missing `sops`/`age` binary, or wrong/missing key path, yields a clear error naming the searched locations.

---

## 7. Security Considerations

| Threat                                 | Handling                                    |
| -------------------------------------- | ------------------------------------------- |
| Secret in version control              | Only the ciphertext file is committed; the age key never is |
| Secret on a sandbox                    | Never sent; tapes see minted tokens only    |
| Secret in logs                         | Values are `SecretString`; always redacted  |
| Private key in `~/Library`/`~/.config` | `chmod 600`; excluded from version control  |

---

## 8. Future Considerations

| Feature              | Description                                    |
| -------------------- | ---------------------------------------------- |
| Multiple recipients  | Encrypt to a team of age keys for shared operation |
| Hardware-backed key  | An age plugin for a YubiKey / secure-enclave identity |
