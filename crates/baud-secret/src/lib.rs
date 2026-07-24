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

    /// Access the inner value. Every call site is visible in code review.
    pub fn expose(&self) -> &T {
        &self.inner
    }
}

/// Debug always emits [REDACTED].
impl<T: Zeroize + Clone> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{REDACTED}")
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

#[derive(Debug, thiserror::Error)]
#[error("required secret '{var}' not found (checked {var}_FILE and {var})")]
pub struct MissingSecret {
    pub var: String,
}

/// Load a secret from the environment using the baud file-convention:
/// 1. Check `{VAR}_FILE`: read that path, strip one trailing newline.
/// 2. Fall back to `{VAR}`.
/// Returns None if neither is set.
pub fn load_secret_env(var: &str) -> Option<SecretString> {
    let file_var = format!("{}_FILE", var);
    if let Ok(path) = std::env::var(&file_var) {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let trimmed = contents.strip_suffix('\n').unwrap_or(&contents).to_owned();
            return Some(Secret::new(trimmed));
        }
    }
    if let Ok(value) = std::env::var(var) {
        return Some(Secret::new(value));
    }
    None
}

/// Like `load_secret_env` but returns an error if neither is set.
pub fn require_secret_env(var: &str) -> Result<SecretString, MissingSecret> {
    load_secret_env(var).ok_or_else(|| MissingSecret {
        var: var.to_owned(),
    })
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
        assert_eq!(format!("{:?}", s), REDACTED);
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
        assert_eq!(format!("{:?}", c), REDACTED);
        assert_eq!(c.expose(), "secret");
    }
}
