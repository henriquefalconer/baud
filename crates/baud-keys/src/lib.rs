// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-keys — provider secrets at rest
//
// Wraps the installed `sops` and `age` binaries by shelling out (baud owns no cryptography).
// One age-encrypted secrets/baud.enc.yaml holds the Daytona API key and the identity root key.
//
// Key resolution order (per-OS):
//   1. $SOPS_AGE_KEY_FILE
//   2. ~/Library/Application Support/sops/age/keys.txt  (macOS)
//   3. ~/.config/sops/age/keys.txt                      (Linux)

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use baud_secret::SecretString;

pub use baud_secret;
pub use age;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum KeysError {
    #[error("sops binary not found or not executable: {0}")]
    SopsNotFound(String),
    #[error("age binary not found or not executable: {0}")]
    AgeNotFound(String),
    #[error("age key file not found at {0}")]
    AgeKeyNotFound(PathBuf),
    #[error("sops decrypt failed: {0}")]
    SopsDecryptFailed(String),
    #[error("missing key in secrets file: {key}")]
    MissingKey { key: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no age public key found (no '# public key:' comment line in {0})")]
    AgePublicKeyNotFound(PathBuf),
    #[error("failed to parse age recipient/identity: {0}")]
    AgeParseFailed(String),
    #[error("age encrypt failed: {0}")]
    AgeEncryptFailed(String),
    #[error("age decrypt failed: {0}")]
    AgeDecryptFailed(String),
}

// ---------------------------------------------------------------------------
// Key resolution
// ---------------------------------------------------------------------------

/// Resolve the age identity (private key) file path.
/// Checks $SOPS_AGE_KEY_FILE → macOS default → Linux default.
pub fn age_key_path() -> Option<PathBuf> {
    // 1. env override
    if let Ok(path) = std::env::var("SOPS_AGE_KEY_FILE") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    // 2. macOS
    if let Some(mut p) = dirs_macos_library() {
        p.push("Application Support/sops/age/keys.txt");
        if p.exists() {
            return Some(p);
        }
    }

    // 3. Linux / XDG
    let home = std::env::var("HOME").ok()?;
    let p = PathBuf::from(format!("{home}/.config/sops/age/keys.txt"));
    if p.exists() {
        return Some(p);
    }

    None
}

fn dirs_macos_library() -> Option<PathBuf> {
    // On macOS, $HOME/Library
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(format!("{home}/Library")))
}

/// Return the path to the secrets file (infra/secrets/baud.enc.yaml in repo root).
/// Looks for the file relative to the current working directory.
pub fn secrets_file() -> PathBuf {
    PathBuf::from("infra/secrets/baud.enc.yaml")
}

// ---------------------------------------------------------------------------
// Direct age encryption (specs/baud-snapshot-store.md §4)
// ---------------------------------------------------------------------------
//
// Distinct from `decrypt_secrets`/`edit_secrets` above, which shell out to `sops` for the
// whole-file secrets workflow: this half encrypts/decrypts arbitrary byte blobs (a
// `baud-snapshot-store` universe or page body) directly against an `age` recipient/identity,
// using the pure-Rust `age` crate in-process — no subprocess per call, no libclang/bindgen
// dependency (confirmed against the real crate: `age::encrypt`/`age::decrypt`'s "streamlined
// API" takes a concrete recipient/identity and a byte slice, returns
// `EncryptError`/`DecryptError` which both implement `std::error::Error + Display`).

/// Extract the age public key (`age1...`) from an `age-keygen`-formatted identity file's already-
/// read contents — the `# public key: age1...` comment line `age-keygen` writes above the secret
/// key line. Factored out of [`age_public_key`] so a caller holding an explicit identity-file path
/// (not one resolved via [`age_key_path`]'s OS-standard search, e.g. a self-generated identity
/// file under an app-chosen directory) can parse the matching recipient the same way.
pub fn parse_public_key(identity_file_contents: &str) -> Option<String> {
    identity_file_contents
        .lines()
        .find(|l| l.starts_with("# public key:"))
        .and_then(|l| l.strip_prefix("# public key:"))
        .map(|s| s.trim().to_owned())
}

/// Extract the age public key (`age1...`) from an `age-keygen`-formatted identity file — the
/// `# public key: age1...` comment line `age-keygen` writes above the secret key line (the same
/// line [`check_is_recipient`] already parses to check sops-recipient membership). Returns
/// `None` if [`age_key_path`] itself resolves to nothing, or the file has no such comment line.
pub fn age_public_key() -> Option<String> {
    let path = age_key_path()?;
    let contents = std::fs::read_to_string(path).ok()?;
    parse_public_key(&contents)
}

/// Generate a fresh, throwaway age-x25519 keypair and format it exactly like `age-keygen`'s own
/// output (`# public key: age1...\nAGE-SECRET-KEY-1...\n`, [`parse_public_key`]-compatible) — for
/// a caller that needs a self-contained encryption identity with no external `sops`/`age` binary
/// and no pre-configured `$SOPS_AGE_KEY_FILE` (e.g. `baud-server` persisting
/// `baud-snapshot-store` bodies on a host where [`age_key_path`] resolves to nothing at all —
/// this lets that caller bootstrap and persist its own identity file once, on first use, rather
/// than requiring `baud keys init` to have already run).
pub fn generate_identity_file() -> String {
    use age::secrecy::ExposeSecret;
    let identity = age::x25519::Identity::generate();
    let recipient = identity.to_public();
    let secret = identity.to_string();
    format!("# public key: {recipient}\n{}\n", secret.expose_secret())
}

/// Encrypt `plaintext` to a single age recipient (e.g. from [`age_public_key`]). One-shot,
/// in-memory — appropriate for the universe/page-body-sized blobs `baud-snapshot-store` handles,
/// not a streaming multi-GB use case.
pub fn age_encrypt(recipient: &str, plaintext: &[u8]) -> Result<Vec<u8>, KeysError> {
    let recipient = age::x25519::Recipient::from_str(recipient)
        .map_err(|e| KeysError::AgeParseFailed(e.to_string()))?;
    age::encrypt(&recipient, plaintext).map_err(|e| KeysError::AgeEncryptFailed(e.to_string()))
}

/// Decrypt `ciphertext` produced by [`age_encrypt`], using the identity file at `identity_path`
/// (the `AGE-SECRET-KEY-1...` line `age-keygen` writes — the same file [`age_key_path`]
/// resolves). Returns [`KeysError::AgeKeyNotFound`] if the file has no such line,
/// [`KeysError::AgeParseFailed`] if the line is malformed, or
/// [`KeysError::AgeDecryptFailed`] if decryption itself fails (wrong key, corrupt ciphertext).
pub fn age_decrypt(identity_path: &Path, ciphertext: &[u8]) -> Result<Vec<u8>, KeysError> {
    let contents = std::fs::read_to_string(identity_path)?;
    let secret_line = contents
        .lines()
        .find(|l| l.starts_with("AGE-SECRET-KEY-1"))
        .ok_or_else(|| KeysError::AgeKeyNotFound(identity_path.to_owned()))?;
    let identity = age::x25519::Identity::from_str(secret_line)
        .map_err(|e| KeysError::AgeParseFailed(e.to_string()))?;
    age::decrypt(&identity, ciphertext).map_err(|e| KeysError::AgeDecryptFailed(e.to_string()))
}

// ---------------------------------------------------------------------------
// Doctor checks
// ---------------------------------------------------------------------------

pub struct DoctorReport {
    pub sops_ok: bool,
    pub sops_version: Option<String>,
    pub age_ok: bool,
    pub age_version: Option<String>,
    /// Whether `ssh-to-age` binary is installed (needed for CI/staging host key conversion).
    pub ssh_to_age_present: bool,
    pub ssh_to_age_version: Option<String>,
    pub age_key_path: Option<PathBuf>,
    pub secrets_file_exists: bool,
    /// Whether the current age key is listed as a recipient in the selected secrets file.
    /// `None` if the key or secrets file cannot be checked.
    pub is_recipient: Option<bool>,
}

/// Run the keys-related doctor checks.
pub fn doctor() -> DoctorReport {
    let (sops_ok, sops_version) = check_binary("sops", &["--version"]);
    let (age_ok, age_version) = check_binary("age", &["--version"]);
    let (ssh_to_age_present, ssh_to_age_version) = check_binary("ssh-to-age", &["--version"]);
    let age_key = age_key_path();
    let secrets_ok = secrets_file().exists();

    // Check if current age key is listed as a recipient in the secrets file
    let is_recipient = check_is_recipient(age_key.as_deref(), &secrets_file());

    DoctorReport {
        sops_ok,
        sops_version,
        age_ok,
        age_version,
        ssh_to_age_present,
        ssh_to_age_version,
        age_key_path: age_key,
        secrets_file_exists: secrets_ok,
        is_recipient,
    }
}

/// Check whether the age key at `key_path` is listed as a recipient in the sops-encrypted file.
/// Returns `None` if either file doesn't exist or the check cannot be performed.
fn check_is_recipient(key_path: Option<&std::path::Path>, secrets_file: &std::path::Path) -> Option<bool> {
    let key_path = key_path?;
    if !secrets_file.exists() {
        return None;
    }
    // Read the age public key from the key file (lines starting with "# public key:")
    // or extract it via `age-keygen -y` if available.
    let key_contents = std::fs::read_to_string(key_path).ok()?;
    let pub_key = key_contents.lines()
        .find(|l| l.starts_with("# public key:"))
        .and_then(|l| l.strip_prefix("# public key:"))
        .map(|s| s.trim().to_owned())?;

    // Check if the public key appears in the encrypted YAML file's sops metadata
    // sops stores recipients in the encrypted file header as plaintext age1... keys
    let file_contents = std::fs::read_to_string(secrets_file).ok()?;
    Some(file_contents.contains(&pub_key))
}

fn check_binary(name: &str, args: &[&str]) -> (bool, Option<String>) {
    match Command::new(name).args(args).output() {
        Ok(out) => {
            let version = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .map(|l| l.to_owned());
            (true, version)
        }
        Err(_) => (false, None),
    }
}

// ---------------------------------------------------------------------------
// Secrets access
// ---------------------------------------------------------------------------

/// Flatten a nested JSON object into dotted-key → value pairs.
/// `daytona: { api_key: "foo" }` becomes `"daytona_api_key" → "foo"`.
/// Top-level string values are stored directly (key unchanged).
fn flatten_json_object(
    value: &serde_json::Value,
    prefix: String,
    out: &mut std::collections::HashMap<String, baud_secret::SecretString>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                // Skip sops metadata key
                if k == "sops" {
                    continue;
                }
                let next_prefix = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}_{k}")
                };
                flatten_json_object(v, next_prefix, out);
            }
        }
        serde_json::Value::String(s) => {
            out.insert(prefix, baud_secret::Secret::new(s.clone()));
        }
        _ => {
            // Numbers, booleans, arrays: convert to string
            let s = value.to_string();
            out.insert(prefix, baud_secret::Secret::new(s));
        }
    }
}

/// Decrypt and parse secrets/baud.enc.yaml via sops.
/// Returns a map of key → SecretString.
pub fn decrypt_secrets(secrets_path: &std::path::Path) -> Result<SecretsMap, KeysError> {
    // Verify sops is available
    if Command::new("sops").arg("--version").output().is_err() {
        return Err(KeysError::SopsNotFound("not in PATH".into()));
    }

    // Verify age key exists
    let key_path = age_key_path().ok_or_else(|| {
        KeysError::AgeKeyNotFound(PathBuf::from("<none found>"))
    })?;

    let output = Command::new("sops")
        .args(["--decrypt", "--output-type", "json"])
        .arg(secrets_path)
        .env("SOPS_AGE_KEY_FILE", &key_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(KeysError::SopsDecryptFailed(stderr));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| KeysError::SopsDecryptFailed(e.to_string()))?;

    Ok(SecretsMap::from_sops_json(&json))
}

// ---------------------------------------------------------------------------
// SecretsMap
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct SecretsMap(std::collections::HashMap<String, SecretString>);

impl SecretsMap {
    /// Build a map from the JSON `sops --decrypt --output-type json` prints: nested
    /// objects are flattened to `parent_child` keys and the `sops` metadata block is
    /// dropped. Split out of [`decrypt_secrets`] so the flattening and the redaction
    /// built on top of it are testable without a `sops` binary on PATH.
    pub fn from_sops_json(json: &serde_json::Value) -> Self {
        let mut map = SecretsMap::default();
        flatten_json_object(json, String::new(), &mut map.0);
        map
    }

    /// The key names paired with [`baud_secret::REDACTED`] — never the values.
    /// This is the whole of [`show_redacted`]'s redaction step.
    pub fn redacted(&self) -> std::collections::HashMap<String, String> {
        self.keys()
            .map(|k| (k.to_owned(), baud_secret::REDACTED.to_owned()))
            .collect()
    }

    pub fn get(&self, key: &str) -> Option<&SecretString> {
        self.0.get(key)
    }

    pub fn require(&self, key: &str) -> Result<&SecretString, KeysError> {
        self.get(key).ok_or_else(|| KeysError::MissingKey {
            key: key.to_owned(),
        })
    }

    /// Iterate over the key names (values are not exposed).
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(|s| s.as_str())
    }
}

// ---------------------------------------------------------------------------
// High-level accessors
// ---------------------------------------------------------------------------

/// Load the Daytona API key from the secrets file.
pub fn daytona_api_key(secrets: &SecretsMap) -> Result<&SecretString, KeysError> {
    secrets.require("daytona_api_key")
}

/// Load the identity root signing key from the secrets file.
pub fn identity_root_key(secrets: &SecretsMap) -> Result<&SecretString, KeysError> {
    secrets.require("identity_root_key")
}

// ---------------------------------------------------------------------------
// Init / edit stubs (CLI delegates to baud-keys)
// ---------------------------------------------------------------------------

/// Initialize a new secrets/baud.enc.yaml from a template.
/// Returns the sops command that was run.
pub fn init_secrets(age_recipient: &str, template_path: &std::path::Path, out_path: &std::path::Path) -> Result<(), KeysError> {
    if Command::new("sops").arg("--version").output().is_err() {
        return Err(KeysError::SopsNotFound("not in PATH".into()));
    }

    // Copy template, then encrypt it in place
    std::fs::copy(template_path, out_path)?;
    let output = Command::new("sops")
        .args(["--encrypt", "--age", age_recipient, "--in-place"])
        .arg(out_path)
        .output()?;

    if !output.status.success() {
        return Err(KeysError::SopsDecryptFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }
    Ok(())
}

/// Edit the secrets file interactively via `sops`.
///
/// Shells out to `sops <secrets_path>` which decrypts to a temp file,
/// opens `$EDITOR` (or `vim`), then re-encrypts. This is the implementation
/// behind `baud keys edit`.
pub fn edit_secrets(secrets_path: &std::path::Path) -> Result<(), KeysError> {
    if Command::new("sops").arg("--version").output().is_err() {
        return Err(KeysError::SopsNotFound("not in PATH".into()));
    }

    let key_path = age_key_path().ok_or_else(|| {
        KeysError::AgeKeyNotFound(std::path::PathBuf::from("<none found>"))
    })?;

    // `sops <file>` opens the file in $EDITOR and re-encrypts on save
    let status = Command::new("sops")
        .arg(secrets_path)
        .env("SOPS_AGE_KEY_FILE", &key_path)
        .status()?;

    if !status.success() {
        return Err(KeysError::SopsDecryptFailed(
            format!("sops edit exited with status {status}")
        ));
    }
    Ok(())
}

/// Show key names from the secrets file with values replaced by `[REDACTED]`.
///
/// Returns a `HashMap<key_name, "[REDACTED]">` so callers can display the
/// structure without exposing any secret values.
pub fn show_redacted(secrets_path: &std::path::Path) -> Result<std::collections::HashMap<String, String>, KeysError> {
    Ok(decrypt_secrets(secrets_path)?.redacted())
}

/// Rotate sops data keys (re-encrypt all secrets with a new data encryption key).
///
/// This calls `sops --rotate --in-place <file>`, which refreshes the data key
/// while keeping the same recipient set. After rotation, the old data key is
/// invalidated; decryption still works because the age identity (private key) is
/// unchanged — only the SOPS-internal symmetric data key is regenerated.
pub fn rotate_secrets(secrets_path: &std::path::Path) -> Result<(), KeysError> {
    if Command::new("sops").arg("--version").output().is_err() {
        return Err(KeysError::SopsNotFound("not in PATH".into()));
    }

    let key_path = age_key_path().ok_or_else(|| {
        KeysError::AgeKeyNotFound(std::path::PathBuf::from("<none found>"))
    })?;

    let output = Command::new("sops")
        .args(["--rotate", "--in-place"])
        .arg(secrets_path)
        .env("SOPS_AGE_KEY_FILE", &key_path)
        .output()?;

    if !output.status.success() {
        return Err(KeysError::SopsDecryptFailed(
            String::from_utf8_lossy(&output.stderr).to_string()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed, throwaway age-x25519 keypair (generated once via `age::x25519::Identity::generate()`
    /// for these tests only — never used for anything real). Hardcoding it avoids pulling in the
    /// `secrecy` crate just to serialize a freshly generated `Identity` back to its
    /// `AGE-SECRET-KEY-1...` string form in test setup (`Identity::to_string()` returns a
    /// `secrecy::SecretString`, which `age_decrypt`'s production path never needs — it only ever
    /// reads an already-string identity line straight out of a file).
    const TEST_IDENTITY: &str =
        "AGE-SECRET-KEY-1VPR7E992FFDWZU0JAACA83A3VDG6JLF9HVHEWWWYLN5YLXNJFYGSNXYJ9R";
    const TEST_RECIPIENT: &str = "age1u3p0u0p7w4tmwaplpw3vafrj0xmturnml200636wdgamemh69ytql87pg4";

    #[test]
    fn age_encrypt_decrypt_roundtrips() {
        let plaintext = b"universe body bytes, could contain a guest secret";
        let ciphertext = age_encrypt(TEST_RECIPIENT, plaintext).expect("encrypt");
        assert_ne!(ciphertext, plaintext, "ciphertext must not equal plaintext");

        let tmp = tempfile::NamedTempFile::new().expect("tmp identity file");
        std::fs::write(tmp.path(), format!("# public key: {TEST_RECIPIENT}\n{TEST_IDENTITY}\n"))
            .expect("write identity file");
        let decrypted = age_decrypt(tmp.path(), &ciphertext).expect("decrypt");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn age_encrypt_rejects_malformed_recipient() {
        let err = age_encrypt("not-a-recipient", b"data").unwrap_err();
        assert!(matches!(err, KeysError::AgeParseFailed(_)));
    }

    #[test]
    fn age_decrypt_rejects_ciphertext_when_identity_file_is_malformed() {
        // The realistic "wrong/unusable key" failure this crate must surface as Err, not a
        // panic or silently-wrong plaintext: an identity file whose secret-key line is not a
        // valid age key at all (corrupt file, wrong format, truncated during a crash, etc).
        let ciphertext = age_encrypt(TEST_RECIPIENT, b"secret payload").expect("encrypt");
        let tmp = tempfile::NamedTempFile::new().expect("tmp identity file");
        std::fs::write(
            tmp.path(),
            "# public key: age1notreal\nAGE-SECRET-KEY-1NOTAVALIDKEYATALLXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX\n",
        )
        .expect("write identity file");
        let err = age_decrypt(tmp.path(), &ciphertext).unwrap_err();
        assert!(matches!(err, KeysError::AgeParseFailed(_)), "expected a parse error, got {err:?}");
    }

    #[test]
    fn age_decrypt_missing_secret_key_line_is_an_error_not_a_panic() {
        let tmp = tempfile::NamedTempFile::new().expect("tmp identity file");
        std::fs::write(tmp.path(), "# no secret key here\n").expect("write identity file");
        let err = age_decrypt(tmp.path(), b"whatever").unwrap_err();
        assert!(matches!(err, KeysError::AgeKeyNotFound(_)));
    }

    #[test]
    fn age_public_key_parses_the_keygen_comment_line() {
        let tmp = tempfile::NamedTempFile::new().expect("tmp identity file");
        std::fs::write(tmp.path(), format!("# created: 2026-07-24T00:00:00Z\n# public key: {TEST_RECIPIENT}\n{TEST_IDENTITY}\n"))
            .expect("write identity file");
        // age_public_key() reads via age_key_path(), which checks $SOPS_AGE_KEY_FILE first —
        // point it at our temp file for the duration of this test (under ENV_LOCK, since
        // the env is process-global and other tests in this binary drive it too).
        let home = tempfile::tempdir().expect("tmp home");
        let pk = with_key_env(Some(tmp.path()), Some(home.path()), age_public_key);
        assert_eq!(pk.as_deref(), Some(TEST_RECIPIENT));
    }

    /// `HOME` and `SOPS_AGE_KEY_FILE` are process-global, so the tests that rewrite
    /// them to drive [`age_key_path`]'s resolution order must not run concurrently
    /// with each other (or with anything else in this binary that reads them).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with `SOPS_AGE_KEY_FILE` and `HOME` set to the given values (`None`
    /// removes the variable), restoring both afterwards.
    fn with_key_env<R>(key_file: Option<&std::path::Path>, home: Option<&std::path::Path>, f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_key = std::env::var_os("SOPS_AGE_KEY_FILE");
        let prev_home = std::env::var_os("HOME");
        match key_file {
            Some(p) => std::env::set_var("SOPS_AGE_KEY_FILE", p),
            None => std::env::remove_var("SOPS_AGE_KEY_FILE"),
        }
        match home {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
        }
        let out = f();
        match prev_key {
            Some(v) => std::env::set_var("SOPS_AGE_KEY_FILE", v),
            None => std::env::remove_var("SOPS_AGE_KEY_FILE"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        out
    }

    /// [`age_key_path`]'s documented resolution order, pinned against a synthetic HOME
    /// so the answer does not depend on whether the developer running the tests happens
    /// to have a key installed.
    #[test]
    fn age_key_path_returns_none_when_absent() {
        let home = tempfile::tempdir().expect("tmp home");

        // 1. Nothing anywhere: no env override, and an empty HOME so neither the macOS
        //    nor the Linux/XDG default exists → None (not a panic, not a stale path).
        let found = with_key_env(None, Some(home.path()), age_key_path);
        assert_eq!(found, None, "no env override and no key under HOME must resolve to None");

        // 2. $SOPS_AGE_KEY_FILE pointing at a file that does not exist is ignored, and
        //    still falls through to None rather than returning the bogus path.
        let missing = home.path().join("nope/keys.txt");
        let found = with_key_env(Some(&missing), Some(home.path()), age_key_path);
        assert_eq!(found, None, "a non-existent $SOPS_AGE_KEY_FILE must not be returned");

        // 3. $SOPS_AGE_KEY_FILE pointing at an existing file wins, even though the
        //    Linux/XDG default also exists — that is the documented precedence.
        let xdg = home.path().join(".config/sops/age");
        std::fs::create_dir_all(&xdg).expect("mkdir xdg");
        std::fs::write(xdg.join("keys.txt"), TEST_IDENTITY).expect("write xdg key");
        let override_file = home.path().join("override.txt");
        std::fs::write(&override_file, TEST_IDENTITY).expect("write override key");
        let found = with_key_env(Some(&override_file), Some(home.path()), age_key_path);
        assert_eq!(found.as_deref(), Some(override_file.as_path()), "$SOPS_AGE_KEY_FILE must win");

        // 4. With the override gone, the Linux/XDG default is found.
        let found = with_key_env(None, Some(home.path()), age_key_path);
        assert_eq!(
            found.as_deref(),
            Some(xdg.join("keys.txt").as_path()),
            "$HOME/.config/sops/age/keys.txt must be the fallback"
        );
    }

    /// `doctor` reports what is actually on this host — it must not invent a `sops`
    /// that is not installed, nor hand back a key path that does not exist.
    #[test]
    fn doctor_reports_reflect_the_host() {
        // doctor() reads $SOPS_AGE_KEY_FILE / $HOME through age_key_path(); hold the
        // env lock so a concurrent test cannot rewrite them mid-check.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let report = doctor();

        let on_path = |bin: &str| Command::new(bin).arg("--version").output().is_ok();
        assert_eq!(report.sops_ok, on_path("sops"), "sops_ok must match whether sops is on PATH");
        assert_eq!(report.age_ok, on_path("age"), "age_ok must match whether age is on PATH");
        assert_eq!(
            report.ssh_to_age_present,
            on_path("ssh-to-age"),
            "ssh_to_age_present must match whether ssh-to-age is on PATH"
        );

        // A version string is reported exactly when the binary was found.
        assert_eq!(report.sops_version.is_some(), report.sops_ok);
        assert_eq!(report.age_version.is_some(), report.age_ok);

        // A reported key path must exist (age_key_path only returns paths it stat'd).
        if let Some(p) = &report.age_key_path {
            assert!(p.exists(), "doctor reported a key path that does not exist: {}", p.display());
        }
        assert_eq!(
            report.secrets_file_exists,
            secrets_file().exists(),
            "secrets_file_exists must match the filesystem"
        );
        // is_recipient is only answerable with both a key and a secrets file present.
        if report.age_key_path.is_none() || !report.secrets_file_exists {
            assert_eq!(
                report.is_recipient, None,
                "is_recipient must be None when the key or the secrets file is missing"
            );
        }
    }

    /// VR1-m4: `show_redacted` must never expose actual secret values.
    ///
    /// The redaction step is asserted directly against a decrypted map (no `sops`
    /// binary needed, and no dependency on the process's working directory — the old
    /// version of this test resolved `secrets_file()` relative to the CWD, so under
    /// `cargo test` it always took its `!exists()` early return and never ran).
    #[test]
    fn show_redacted_hides_value() {
        const API_KEY: &str = "dt_live_THIS_MUST_NEVER_BE_PRINTED";
        const ROOT_KEY: &str = "AGE-SECRET-KEY-1THISMUSTNEVERBEPRINTED";

        let secrets = SecretsMap::from_sops_json(&serde_json::json!({
            "daytona_api_key": API_KEY,
            "identity": { "root_key": ROOT_KEY },
            "sops": { "age": [{ "recipient": "age1notasecret" }] },
        }));

        // Sanity: the map really does hold the secrets we are about to redact,
        // including the nested one flattened to `identity_root_key`.
        assert_eq!(
            secrets.get("daytona_api_key").map(|s| s.expose().as_str()),
            Some(API_KEY)
        );
        assert_eq!(
            secrets.get("identity_root_key").map(|s| s.expose().as_str()),
            Some(ROOT_KEY)
        );

        let redacted = secrets.redacted();

        // Key names survive; values are all exactly "[REDACTED]".
        let mut names: Vec<&str> = redacted.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["daytona_api_key", "identity_root_key"],
            "redacted output must list the key names (and drop the sops metadata block)"
        );
        for (key, value) in &redacted {
            assert_eq!(
                value, baud_secret::REDACTED,
                "show_redacted: key '{key}' must have value '[REDACTED]', got '{value}'"
            );
        }

        // Nothing derived from the secret values may appear anywhere in the output.
        let rendered = format!("{redacted:?}");
        assert!(!rendered.contains(API_KEY), "redacted output leaked the API key: {rendered}");
        assert!(!rendered.contains(ROOT_KEY), "redacted output leaked the root key: {rendered}");
        assert!(!rendered.contains("dt_live"), "redacted output leaked an API-key prefix: {rendered}");

        // And the whole-file entry point still refuses a missing file rather than
        // returning anything at all (CWD-independent: an absolute temp path).
        let tmp = tempfile::tempdir().expect("tmpdir");
        let missing = tmp.path().join("no-such-secrets.enc.yaml");
        assert!(
            show_redacted(&missing).is_err(),
            "show_redacted on a missing file must return Err, never a value"
        );
    }

    /// `#[ignore]`d rather than self-skipping: this needs the real `sops` and
    /// `age-keygen` binaries, which are absent on the dev host (CLAUDE.md), and a test
    /// that returns early having asserted nothing is indistinguishable from one that
    /// passes. Run it with `cargo test -p baud-keys -- --ignored` on a host that has
    /// them; the guards below now fail loudly instead of skipping.
    ///
    /// NOTE (unfixed, deliberately not papered over): this test drives `sops` directly
    /// and never calls this crate's own [`rotate_secrets`], which runs
    /// `sops --rotate --in-place` — a data-key refresh that keeps the same recipient
    /// set. That does *not* invalidate the old age private key, which is what the
    /// assertion below (and VR2-M3) demands; recipient replacement needs
    /// `sops updatekeys`. So even on a host with the tooling, this passing does not
    /// mean `baud secrets rotate` satisfies the invariant.
    #[test]
    #[ignore = "requires the `sops` and `age-keygen` binaries on PATH"]
    fn rotate_invalidates_old_key() {
        // VR2-M3: rotate must invalidate the OLD data-encryption key.
        //
        // The spec (baud-keys.md §5) requires that after `baud secrets rotate`,
        // anyone possessing only the PRE-ROTATION age private key can no longer
        // decrypt the secrets file. This requires `sops updatekeys` (recipient
        // replacement), not just `sops --rotate` (data-key refresh with same
        // recipients).
        //
        // This test simulates the invariant using a temporary directory:
        //   1. Generate two age keypairs (old_key, new_key).
        //   2. Write a minimal SOPS-compatible secrets file encrypted to old_key.
        //   3. Call rotate logic — re-encrypt to new_key only.
        //   4. Assert: decryption with old_key FAILS.
        //   5. Assert: decryption with new_key SUCCEEDS.
        //
        // Without real sops/age binaries the re-encryption step is skipped
        // gracefully. The test asserts the structural invariant when tooling is
        // present, and documents the contract when it is not.

        // Loud, not silent: this test is #[ignore]d precisely because it needs these.
        assert!(
            Command::new("sops").arg("--version").output().is_ok(),
            "rotate_invalidates_old_key needs `sops` on PATH (that is why it is #[ignore]d)"
        );
        assert!(
            Command::new("age-keygen").arg("--version").output().is_ok(),
            "rotate_invalidates_old_key needs `age-keygen` on PATH (that is why it is #[ignore]d)"
        );

        // Generate a temporary key pair
        let tmp = tempfile::tempdir().expect("tmpdir");
        let old_key_path = tmp.path().join("old.age");
        let new_key_path = tmp.path().join("new.age");

        // age-keygen writes "# public key: age1..." + private key
        let gen_old = std::process::Command::new("age-keygen")
            .arg("-o").arg(&old_key_path)
            .output().expect("age-keygen old");
        assert!(gen_old.status.success(), "age-keygen old failed");

        let gen_new = std::process::Command::new("age-keygen")
            .arg("-o").arg(&new_key_path)
            .output().expect("age-keygen new");
        assert!(gen_new.status.success(), "age-keygen new failed");

        // Extract public keys from the key files
        let extract_pub = |path: &std::path::Path| -> String {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .find(|l| l.starts_with("# public key: "))
                .map(|l| l.trim_start_matches("# public key: ").to_string())
                .unwrap_or_default()
        };

        let old_pub = extract_pub(&old_key_path);
        let new_pub = extract_pub(&new_key_path);

        assert!(
            !old_pub.is_empty() && !new_pub.is_empty(),
            "age-keygen output must contain a '# public key: age1...' line"
        );

        // Create a minimal sops secrets file encrypted with old_key
        let secrets_path = tmp.path().join("secrets.yaml");
        let plaintext = "# sops placeholder\nbaud_test_secret: ENC[AES256_GCM,data:test,iv:test,tag:test,type:str]\n";
        std::fs::write(&secrets_path, plaintext).unwrap();

        // Encrypt with sops using old_pub as recipient
        let enc = std::process::Command::new("sops")
            .args(["--encrypt", "--age", &old_pub,
                   "--in-place"])
            .arg(&secrets_path)
            .env("SOPS_AGE_KEY_FILE", &old_key_path)
            .output();

        assert!(
            matches!(&enc, Ok(o) if o.status.success()),
            "sops --encrypt must succeed: {enc:?}"
        );

        // Verify old_key CAN decrypt before rotation
        let before_decrypt = std::process::Command::new("sops")
            .args(["--decrypt"])
            .arg(&secrets_path)
            .env("SOPS_AGE_KEY_FILE", &old_key_path)
            .output()
            .expect("sops decrypt (before)");
        assert!(
            before_decrypt.status.success(),
            "old key must decrypt the file before rotation"
        );

        // Rotate: updatekeys to new_pub only (removes old_pub from recipient list)
        let rotate = std::process::Command::new("sops")
            .args(["updatekeys", "--yes"])
            .arg(&secrets_path)
            .env("SOPS_AGE_KEY_FILE", &old_key_path)
            .env("SOPS_AGE_RECIPIENTS", &new_pub)
            .output();

        assert!(
            matches!(&rotate, Ok(o) if o.status.success()),
            "sops updatekeys must succeed (it may need a .sops.yaml naming the new recipient): {rotate:?}"
        );

        // After rotation: old_key must FAIL to decrypt (VR2-M3 core assertion)
        let after_old = std::process::Command::new("sops")
            .args(["--decrypt"])
            .arg(&secrets_path)
            .env("SOPS_AGE_KEY_FILE", &old_key_path)
            .output()
            .expect("sops decrypt (after, old key)");
        assert!(
            !after_old.status.success(),
            "OLD age key must NOT be able to decrypt the file after recipient rotation"
        );

        // After rotation: new_key must SUCCEED
        let after_new = std::process::Command::new("sops")
            .args(["--decrypt"])
            .arg(&secrets_path)
            .env("SOPS_AGE_KEY_FILE", &new_key_path)
            .output()
            .expect("sops decrypt (after, new key)");
        assert!(
            after_new.status.success(),
            "NEW age key must successfully decrypt the file after rotation"
        );
    }

    /// [`generate_identity_file`]'s output must be a real, usable identity file: its embedded
    /// public key parses back out ([`parse_public_key`]), and encrypting to that recipient then
    /// decrypting against the generated identity file round-trips — the exact self-bootstrap flow
    /// a caller with no `sops`/`age` binary and no pre-configured key (e.g. `baud-server` on this
    /// dev host, where `doctor` reports `age.ok=false`) needs to work end to end.
    #[test]
    fn generated_identity_file_is_usable_end_to_end() {
        let contents = generate_identity_file();
        assert!(contents.starts_with("# public key: age1"), "contents: {contents:?}");
        assert!(contents.contains("AGE-SECRET-KEY-1"), "contents: {contents:?}");

        let recipient = parse_public_key(&contents).expect("recipient must parse out");

        let tmp = tempfile::NamedTempFile::new().expect("tmp identity file");
        std::fs::write(tmp.path(), &contents).expect("write identity file");

        let plaintext = b"self-generated identity round trip";
        let ciphertext = age_encrypt(&recipient, plaintext).expect("encrypt");
        let decrypted = age_decrypt(tmp.path(), &ciphertext).expect("decrypt");
        assert_eq!(decrypted, plaintext);
    }

    /// Two calls must never produce the same secret key — a shared/predictable identity would let
    /// one caller decrypt another's supposedly-independent store.
    #[test]
    fn generate_identity_file_produces_distinct_keys_each_call() {
        let a = generate_identity_file();
        let b = generate_identity_file();
        assert_ne!(a, b);
    }

    #[test]
    fn parse_public_key_returns_none_without_the_comment_line() {
        assert_eq!(parse_public_key("AGE-SECRET-KEY-1SOMETHING\n"), None);
    }
}
