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

/// Negotiate the live CoW capability against an actual mapping. Callers must keep the mapping
/// alive while they service faults. Unsupported kernels return an explicit full-restore mode,
/// never a mode that claims write-set scaling.
#[cfg(target_os = "linux")]
pub fn negotiate_shared_cow(
    mapping: &crate::backing::PrivateCowMapping,
) -> Result<crate::userfaultfd::CowRegion, BranchMode> {
    crate::userfaultfd::CowRegion::open(mapping.as_ptr() as u64, mapping.len() as u64).map_err(
        |error| {
            let fallback = match error {
                crate::userfaultfd::Error::MissingFeature(_) => {
                    FallbackReason::RequiredCapabilityMissing
                }
                crate::userfaultfd::Error::MissingIoctl(_) => {
                    FallbackReason::RequiredCapabilityMissing
                }
                crate::userfaultfd::Error::Io(_) => FallbackReason::UserfaultfdUnavailable,
                crate::userfaultfd::Error::Api(_, _) => FallbackReason::RequiredCapabilityMissing,
                crate::userfaultfd::Error::UnalignedRange
                | crate::userfaultfd::Error::UnexpectedEvent(_)
                | crate::userfaultfd::Error::ShortRead => FallbackReason::MappingFailed,
            };
            BranchMode::FullRestore { fallback }
        },
    )
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
