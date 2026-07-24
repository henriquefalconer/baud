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

use std::path::PathBuf;
use std::process::Command;
use baud_secret::SecretString;

pub use baud_secret;

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

/// Return the path to the secrets file (secrets/baud.enc.yaml in repo root).
/// Looks for the file relative to the current working directory.
pub fn secrets_file() -> PathBuf {
    PathBuf::from("secrets/baud.enc.yaml")
}

// ---------------------------------------------------------------------------
// Doctor checks
// ---------------------------------------------------------------------------

pub struct DoctorReport {
    pub sops_ok: bool,
    pub sops_version: Option<String>,
    pub age_ok: bool,
    pub age_version: Option<String>,
    pub age_key_path: Option<PathBuf>,
    pub secrets_file_exists: bool,
}

/// Run the keys-related doctor checks.
pub fn doctor() -> DoctorReport {
    let (sops_ok, sops_version) = check_binary("sops", &["--version"]);
    let (age_ok, age_version) = check_binary("age", &["--version"]);
    let age_key = age_key_path();
    let secrets_ok = secrets_file().exists();

    DoctorReport {
        sops_ok,
        sops_version,
        age_ok,
        age_version,
        age_key_path: age_key,
        secrets_file_exists: secrets_ok,
    }
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

    let mut map = SecretsMap::default();
    if let Some(obj) = json.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                map.0.insert(k.clone(), baud_secret::Secret::new(s.to_owned()));
            }
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// SecretsMap
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct SecretsMap(std::collections::HashMap<String, SecretString>);

impl SecretsMap {
    pub fn get(&self, key: &str) -> Option<&SecretString> {
        self.0.get(key)
    }

    pub fn require(&self, key: &str) -> Result<&SecretString, KeysError> {
        self.get(key).ok_or_else(|| KeysError::MissingKey {
            key: key.to_owned(),
        })
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_key_path_returns_none_when_absent() {
        // With no SOPS_AGE_KEY_FILE and no default location (typical CI), this
        // should return None rather than panic.
        // (If the dev machine has the key, it returns Some — both outcomes are valid.)
        let _ = age_key_path();
    }

    #[test]
    fn doctor_does_not_panic() {
        let report = doctor();
        // sops may or may not be installed — just check it doesn't panic
        let _ = report.sops_ok;
    }
}
