// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use std::sync::Arc;

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

/// A live branch memory owner. It is intentionally process-local and cannot be serialized into a
/// `Universe`. The parent memfd and shared mapping stay alive while KVM uses the branch mapping;
/// UFFD faults turn the first write to each page into a private page and record the write set.
#[cfg(target_os = "linux")]
pub struct LiveCowBranch {
    backing: Arc<crate::backing::GuestRamBacking>,
    /// The registered destination mapping is write-protected. A separate read-only source
    /// mapping is required for UFFDIO_CONTINUE/COPY because the destination page is unresolved or
    /// already private when the fault arrives; using the faulting address as both source and
    /// destination can feed an unresolved page back into the kernel and deadlock the vCPU.
    mapping: crate::backing::PrivateCowMapping,
    source: crate::backing::PrivateCowMapping,
    uffd: crate::userfaultfd::CowRegion,
    page_size: usize,
    dirty_pages: std::collections::BTreeSet<usize>,
}

#[cfg(target_os = "linux")]
impl LiveCowBranch {
    pub fn open(backing: Arc<crate::backing::GuestRamBacking>) -> Result<Self, BranchMode> {
        let mapping = backing.map_shared().map_err(|_| BranchMode::FullRestore {
            fallback: FallbackReason::MappingFailed,
        })?;
        let source = backing.map_shared().map_err(|_| BranchMode::FullRestore {
            fallback: FallbackReason::MappingFailed,
        })?;
        let uffd = negotiate_shared_cow(&mapping)?;
        uffd.write_protect(true)
            .map_err(|_| BranchMode::FullRestore {
                fallback: FallbackReason::RequiredCapabilityMissing,
            })?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        Ok(Self {
            backing,
            mapping,
            source,
            uffd,
            page_size,
            dirty_pages: std::collections::BTreeSet::new(),
        })
    }

    pub fn mapping(&self) -> &crate::backing::PrivateCowMapping {
        &self.mapping
    }

    pub fn dirty_pages(&self) -> impl Iterator<Item = usize> + '_ {
        self.dirty_pages.iter().copied()
    }

    /// Drain all currently available faults. A caller normally runs this on a dedicated worker
    /// while the vCPU uses the registered mapping, and treats any error as a failed branch.
    pub fn service_faults(&mut self) -> Result<usize, crate::userfaultfd::Error> {
        let mut serviced = 0;
        while let Some(fault) = self.uffd.read_fault()? {
            let offset = fault
                .address
                .checked_sub(self.mapping.as_ptr() as u64)
                .ok_or(crate::userfaultfd::Error::UnalignedRange)?
                as usize;
            if offset >= self.mapping.len() {
                return Err(crate::userfaultfd::Error::UnalignedRange);
            }
            let page = offset / self.page_size;
            let address = self.mapping.as_ptr() as u64 + (page * self.page_size) as u64;
            let source = self.source.as_ptr() as u64 + (page * self.page_size) as u64;
            if fault.minor {
                self.uffd
                    .continue_from(address, self.page_size as u64, source)?;
            } else if fault.write_protect && fault.write {
                self.uffd.copy_page(address, source)?;
                self.dirty_pages.insert(page);
            } else {
                return Err(crate::userfaultfd::Error::UnexpectedEvent(0));
            }
            serviced += 1;
        }
        Ok(serviced)
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
