// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use serde::{Deserialize, Serialize};

/// How a live branch obtained guest RAM. Persisted universes must report `FullRestore` because
/// their process-local backing fd cannot be serialized or reopened from the wire format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchMode {
    SharedPrivateCow,
    FullRestore { fallback: FallbackReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    UserfaultfdUnavailable,
    RequiredCapabilityMissing,
    BackingUnavailable,
    MappingFailed,
    UnsupportedHost,
}

impl BranchMode {
    pub fn is_write_set_scaled(&self) -> bool {
        matches!(self, Self::SharedPrivateCow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_explicit_and_never_claims_write_set_scaling() {
        let mode = BranchMode::FullRestore {
            fallback: FallbackReason::BackingUnavailable,
        };
        assert!(!mode.is_write_set_scaled());
        assert!(matches!(
            mode,
            BranchMode::FullRestore {
                fallback: FallbackReason::BackingUnavailable
            }
        ));
    }
}
