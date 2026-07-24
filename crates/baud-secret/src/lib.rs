// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-secret — type-safe secret wrapper
//
// - Secret<T: Zeroize>: inner reachable only via .expose() — visible in code review
// - Debug/Display/Serialize emit "[REDACTED]"
// - Deserialize loads normally
// - Zeroized on drop
// - load_secret_env / require_secret_env: {VAR}_FILE → {VAR} lookup
// - soft budget <= 400 LOC

use std::fmt;
use zeroize::{Zeroize, ZeroizeOnDrop};
use serde::{de::Deserializer, ser::Serializer, Deserialize, Serialize};

pub const REDACTED: &str = "[REDACTED]";

// ---------------------------------------------------------------------------
// Secret<T>
// ---------------------------------------------------------------------------

/// A type-safe secret wrapper. The inner value is only reachable via `.expose()`.
/// Drop zeroizes the inner value.
#[derive(Clone, ZeroizeOnDrop)]
pub struct Secret<T: Zeroize + Clone> {
    inner: T,
}

impl<T: Zeroize + Clone> Secret<T> {
    /// Wrap a value in a Secret.
    pub fn new(value: T) -> Self {
        Secret { inner: value }
    }

    /// Access the inner value (immutable). Every call site is visible in code review.
    pub fn expose(&self) -> &T {
        &self.inner
    }

    /// Access the inner value mutably. Every call site is visible in code review.
    pub fn expose_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Consume the Secret and return the inner value.
    /// The value is NOT zeroized when returned — caller is responsible.
    pub fn into_inner(mut self) -> T where T: Clone {
        let val = self.inner.clone();
        self.inner.zeroize();
        std::mem::forget(self); // avoid double-zeroize in ZeroizeOnDrop
        val
    }
}

/// PartialEq compares the inner values (for testing without exposing secrets).
impl<T: Zeroize + Clone + PartialEq> PartialEq for Secret<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<T: Zeroize + Clone + PartialEq + Eq> Eq for Secret<T> {}

/// Debug emits `Secret("[REDACTED]")` (spec §3 format).
impl<T: Zeroize + Clone> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(\"{REDACTED}\")")
    }
}

/// Display always emits [REDACTED].
impl<T: Zeroize + Clone> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{REDACTED}")
    }
}

/// Serialize always emits the string "[REDACTED]".
impl<T: Zeroize + Clone> Serialize for Secret<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(REDACTED)
    }
}

/// Deserialize loads normally into the inner type.
impl<'de, T: Zeroize + Clone + Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let inner = T::deserialize(d)?;
        Ok(Secret { inner })
    }
}

// ---------------------------------------------------------------------------
// SecretString alias
// ---------------------------------------------------------------------------

/// Convenience alias for the common case of a secret string.
pub type SecretString = Secret<String>;

// ---------------------------------------------------------------------------
// Environment helpers
// ---------------------------------------------------------------------------

/// Errors from `load_secret_env` / `require_secret_env`.
#[derive(Debug, thiserror::Error)]
pub enum SecretEnvError {
    #[error("required secret '{var}' not found (checked {var}_FILE and {var})")]
    Missing { var: String },
    #[error("could not read secret file '{path}': {source}")]
    FileReadError {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Kept for backward compat — alias of `SecretEnvError::Missing`.
#[derive(Debug, thiserror::Error)]
#[error("required secret '{var}' not found (checked {var}_FILE and {var})")]
pub struct MissingSecret {
    pub var: String,
}

/// Load a secret from the environment using the baud file-convention:
/// 1. Check `{VAR}_FILE`: read that path, strip one trailing newline.
///    If the path is set but unreadable → returns `Err(SecretEnvError::FileReadError)`.
/// 2. Fall back to `{VAR}`.
/// Returns `Ok(None)` if neither is set.
pub fn load_secret_env(var: &str) -> Result<Option<SecretString>, SecretEnvError> {
    let file_var = format!("{}_FILE", var);
    if let Ok(path) = std::env::var(&file_var) {
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| SecretEnvError::FileReadError { path: path.clone(), source: e })?;
        let trimmed = contents.strip_suffix('\n').unwrap_or(&contents).to_owned();
        return Ok(Some(Secret::new(trimmed)));
    }
    if let Ok(value) = std::env::var(var) {
        return Ok(Some(Secret::new(value)));
    }
    Ok(None)
}

/// Like `load_secret_env` but returns an error if neither is set.
pub fn require_secret_env(var: &str) -> Result<SecretString, SecretEnvError> {
    load_secret_env(var)?
        .ok_or_else(|| SecretEnvError::Missing { var: var.to_owned() })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = Secret::new("my-super-secret".to_owned());
        // Debug format is Secret("[REDACTED]") per spec §3
        assert_eq!(format!("{:?}", s), format!("Secret(\"{REDACTED}\")"));
    }

    #[test]
    fn display_is_redacted() {
        let s = Secret::new("my-super-secret".to_owned());
        assert_eq!(format!("{}", s), REDACTED);
    }

    #[test]
    fn expose_returns_inner() {
        let s = Secret::new("hello".to_owned());
        assert_eq!(s.expose(), "hello");
    }

    #[test]
    fn serialize_is_redacted() {
        let s = Secret::new("hello".to_owned());
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(j, format!("\"{}\"", REDACTED));
    }

    #[test]
    fn deserialize_loads_normally() {
        let s: SecretString = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(s.expose(), "hello");
    }

    #[test]
    fn clone_does_not_expose() {
        let s = Secret::new("secret".to_owned());
        let c = s.clone();
        assert_eq!(format!("{:?}", c), format!("Secret(\"{REDACTED}\")"));
        assert_eq!(c.expose(), "secret");
    }

    #[test]
    fn expose_mut_allows_mutation() {
        let mut s = Secret::new("hello".to_owned());
        s.expose_mut().push_str(" world");
        assert_eq!(s.expose(), "hello world");
        // Debug still redacted after mutation
        assert_eq!(format!("{:?}", s), format!("Secret(\"{REDACTED}\")"));
    }

    #[test]
    fn into_inner_returns_value() {
        let s = Secret::new("my-secret".to_owned());
        let val = s.into_inner();
        assert_eq!(val, "my-secret");
    }

    #[test]
    fn partial_eq_compares_inner() {
        let a = Secret::new("same".to_owned());
        let b = Secret::new("same".to_owned());
        let c = Secret::new("different".to_owned());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn load_secret_env_returns_result_option() {
        // No env set → Ok(None)
        let result = load_secret_env("BAUD_TEST_UNSET_XYZ_12345");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    // ---------------------------------------------------------------------------
    // Proptest: spec-mandated property tests (specs/baud-secret.md §5)
    // ---------------------------------------------------------------------------

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// debug_never_contains_secret: for any secret string that is not a substring
        /// of the REDACTED marker, the Debug output must not contain the full secret.
        /// Security property: Debug must emit only `Secret("[REDACTED]")` regardless of content.
        proptest! {
            #[test]
            fn debug_never_contains_secret(s in "[A-Za-z0-9]{8,64}") {
                // We use {8,64} to avoid short tokens that appear in "[REDACTED]"
                // (e.g., single chars like "R" or short prefixes like "RED")
                let secret = Secret::new(s.clone());
                let debug_output = format!("{:?}", secret);
                // The debug output must be exactly the spec format
                let expected = format!("Secret(\"{REDACTED}\")");
                prop_assert_eq!(
                    debug_output,
                    expected,
                    "Debug output must be exactly Secret(\"[REDACTED]\")"
                );
            }
        }

        /// serialize_never_contains_secret: for any secret string, JSON serialization
        /// must emit only `"[REDACTED]"` and not the actual value.
        proptest! {
            #[test]
            fn serialize_never_contains_secret(s in "[A-Za-z0-9]{8,64}") {
                let secret = Secret::new(s.clone());
                let json = serde_json::to_string(&secret).unwrap();
                // The serialized output must be exactly the REDACTED marker as a JSON string
                let expected = format!("\"{}\"", REDACTED);
                prop_assert_eq!(
                    json,
                    expected,
                    "Serialized output must be exactly the REDACTED marker as a JSON string"
                );
            }
        }
    }

    #[test]
    fn load_secret_env_file_not_found_returns_error() {
        // Set _FILE to a non-existent path → Err(FileReadError)
        std::env::set_var("BAUD_TEST_FILE_ERR_FILE", "/nonexistent/path/secret.txt");
        let result = load_secret_env("BAUD_TEST_FILE_ERR");
        std::env::remove_var("BAUD_TEST_FILE_ERR_FILE");
        assert!(result.is_err());
        match result.unwrap_err() {
            SecretEnvError::FileReadError { .. } => {}
            other => panic!("expected FileReadError, got {other:?}"),
        }
    }
}
