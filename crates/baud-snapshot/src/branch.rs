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

/// UFFD owner for a mapping created by `GuestMemoryMmap`. KVM must be registered against this
/// exact address, not a second mapping, so this type deliberately does not own or unmap the
/// destination. The caller owns the `GuestMemoryMmap` and keeps it alive for this value's lifetime.
#[cfg(target_os = "linux")]
pub struct ExternalCowBranch {
    _backing: Arc<crate::backing::GuestRamBacking>,
    mapping_start: u64,
    mapping_len: usize,
    source: crate::backing::PrivateCowMapping,
    uffd: crate::userfaultfd::CowRegion,
    page_size: usize,
    dirty_pages: std::collections::BTreeSet<usize>,
}

#[cfg(target_os = "linux")]
unsafe impl Send for ExternalCowBranch {}

#[cfg(target_os = "linux")]
impl ExternalCowBranch {
    /// Register the exact UFFD-managed mapping as a KVM memory slot. The caller must keep this
    /// branch and the mapping owner alive until the VM is destroyed.
    pub fn register_kvm(
        &self,
        vm: &kvm_ioctls::VmFd,
        slot: u32,
        guest_phys_addr: u64,
    ) -> Result<(), kvm_ioctls::Error> {
        let region = kvm_bindings::kvm_userspace_memory_region {
            slot,
            guest_phys_addr,
            memory_size: self.mapping_len as u64,
            userspace_addr: self.mapping_start,
            flags: 0,
        };
        unsafe { vm.set_user_memory_region(region) }
    }

    pub fn open(
        backing: Arc<crate::backing::GuestRamBacking>,
        mapping_start: u64,
        mapping_len: usize,
    ) -> Result<Self, BranchMode> {
        let source = backing.map_shared().map_err(|_| BranchMode::FullRestore {
            fallback: FallbackReason::MappingFailed,
        })?;
        let uffd = negotiate_shared_cow_at(mapping_start, mapping_len)?;
        uffd.write_protect(true)
            .map_err(|_| BranchMode::FullRestore {
                fallback: FallbackReason::RequiredCapabilityMissing,
            })?;
        Ok(Self {
            _backing: backing,
            mapping_start,
            mapping_len,
            source,
            uffd,
            page_size: unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize },
            dirty_pages: std::collections::BTreeSet::new(),
        })
    }

    pub fn mapping_start(&self) -> u64 {
        self.mapping_start
    }
    pub fn mapping_len(&self) -> usize {
        self.mapping_len
    }
    pub fn dirty_pages(&self) -> impl Iterator<Item = usize> + '_ {
        self.dirty_pages.iter().copied()
    }

    pub fn service_faults(&mut self) -> Result<usize, crate::userfaultfd::Error> {
        let mut serviced = 0;
        while let Some(fault) = self.uffd.read_fault()? {
            let offset = fault
                .address
                .checked_sub(self.mapping_start)
                .ok_or(crate::userfaultfd::Error::UnalignedRange)?
                as usize;
            if offset >= self.mapping_len {
                return Err(crate::userfaultfd::Error::UnalignedRange);
            }
            let page = offset / self.page_size;
            let address = self.mapping_start + (page * self.page_size) as u64;
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

    /// The worker must run concurrently with KVM. A faulting vCPU is blocked in the kernel until
    /// this thread resolves the UFFD event, so servicing after `KVM_RUN` returns is incorrect.
    pub fn start_fault_worker(self) -> FaultWorker {
        FaultWorker::start(self)
    }
}

#[cfg(target_os = "linux")]
fn negotiate_shared_cow_at(
    start: u64,
    len: usize,
) -> Result<crate::userfaultfd::CowRegion, BranchMode> {
    crate::userfaultfd::CowRegion::open(start, len as u64).map_err(|error| {
        let fallback = match error {
            crate::userfaultfd::Error::MissingFeature(_)
            | crate::userfaultfd::Error::MissingIoctl(_)
            | crate::userfaultfd::Error::Api(_, _) => FallbackReason::RequiredCapabilityMissing,
            crate::userfaultfd::Error::Io(_) => FallbackReason::UserfaultfdUnavailable,
            crate::userfaultfd::Error::UnalignedRange
            | crate::userfaultfd::Error::UnexpectedEvent(_)
            | crate::userfaultfd::Error::ShortRead => FallbackReason::MappingFailed,
        };
        BranchMode::FullRestore { fallback }
    })
}

#[cfg(target_os = "linux")]
pub struct FaultWorker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<Result<ExternalCowBranch, crate::userfaultfd::Error>>>,
}

#[cfg(target_os = "linux")]
impl FaultWorker {
    fn start(mut branch: ExternalCowBranch) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_stop = stop.clone();
        let join = std::thread::spawn(move || {
            while !thread_stop.load(std::sync::atomic::Ordering::Acquire) {
                if branch.uffd.wait_for_fault(50)? {
                    branch.service_faults()?;
                }
            }
            Ok(branch)
        });
        Self {
            stop,
            join: Some(join),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for FaultWorker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
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

    #[test]
    fn negotiated_shared_mode_reports_write_set_scaling() {
        assert!(BranchMode::SharedPrivateCow.is_write_set_scaled());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invalid_external_mapping_reports_full_restore_instead_of_claiming_cow() {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let backing = std::sync::Arc::new(
            crate::backing::GuestRamBacking::from_bytes(&vec![0; page]).unwrap(),
        );
        let result = ExternalCowBranch::open(backing, 1, page);
        assert!(matches!(
            result,
            Err(BranchMode::FullRestore {
                fallback: FallbackReason::MappingFailed
            })
        ));
    }
}
