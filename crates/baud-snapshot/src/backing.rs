// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

//! Live, process-local guest-RAM backing for private-COW branches.
//!
//! A serialized [`crate::Universe`] cannot carry an fd or mapping, so persisted snapshots keep
//! using the full-restore path. Live branches may share one immutable memfd through MAP_PRIVATE;
//! the kernel then shares clean pages and gives each branch a private page on its first write.
//! This is deliberately separate from the userfaultfd wrapper. A minor-fault continuation on a
//! MAP_SHARED mapping would not isolate later writes.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr::NonNull;
use std::slice;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum BackingError {
    #[error("guest RAM backing must be non-empty and page aligned")]
    InvalidLength,
    #[error("memfd_create failed: {0}")]
    Create(#[source] io::Error),
    #[error("writing guest RAM backing failed: {0}")]
    Write(#[source] io::Error),
    #[error("mapping private guest RAM failed: {0}")]
    Map(#[source] io::Error),
}

/// Immutable source bytes retained for the lifetime of every branch mapping.
#[derive(Clone)]
pub struct GuestRamBacking {
    file: Arc<File>,
    len: usize,
}

impl GuestRamBacking {
    /// Retain a duplicate of an existing memfd-backed guest-RAM file.
    ///
    /// The duplicate is independent of the `File` used by `GuestMemoryMmap`, but refers to the
    /// same file description and page cache. This is the hand-off used by a live multiverse branch.
    pub fn from_file(file: &File, len: usize) -> Result<Self, BackingError> {
        let page = page_size();
        if len == 0 || !len.is_multiple_of(page) {
            return Err(BackingError::InvalidLength);
        }
        let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(BackingError::Create(io::Error::last_os_error()));
        }
        Ok(Self {
            file: Arc::new(unsafe { File::from_raw_fd(fd) }),
            len,
        })
    }

    /// Build one backing file from a complete captured RAM image.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BackingError> {
        let page = page_size();
        if bytes.is_empty() || !bytes.len().is_multiple_of(page) {
            return Err(BackingError::InvalidLength);
        }
        let name = std::ffi::CString::new("baud-guest-ram-cow").expect("literal has no NUL");
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(BackingError::Create(io::Error::last_os_error()));
        }
        let file = unsafe { File::from_raw_fd(raw) };
        file.set_len(bytes.len() as u64)
            .map_err(BackingError::Create)?;
        let mut written = 0;
        while written < bytes.len() {
            let n = unsafe {
                libc::pwrite(
                    file.as_raw_fd(),
                    bytes[written..].as_ptr().cast(),
                    bytes.len() - written,
                    written as libc::off_t,
                )
            };
            if n <= 0 {
                return Err(BackingError::Write(io::Error::last_os_error()));
            }
            written += n as usize;
        }
        Ok(Self {
            file: Arc::new(file),
            len: bytes.len(),
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Map this snapshot privately. Clean pages may be shared by the kernel, while a branch write
    /// is isolated from the source and every other mapping.
    pub fn map_private(&self) -> Result<PrivateCowMapping, BackingError> {
        self.map_with_flags(libc::MAP_PRIVATE)
    }

    /// Map the backing shared. This is the mapping mode required for UFFD minor faults. The
    /// live branch write-protects it before KVM runs, so a first write is copied into a private
    /// page by `UFFDIO_COPY` rather than modifying the memfd.
    pub fn map_shared(&self) -> Result<PrivateCowMapping, BackingError> {
        self.map_with_flags(libc::MAP_SHARED)
    }

    /// Return the file descriptor for integration code that creates its own shared mapping.
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }

    fn map_with_flags(&self, flags: i32) -> Result<PrivateCowMapping, BackingError> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                self.len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                self.file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(BackingError::Map(io::Error::last_os_error()));
        }
        Ok(PrivateCowMapping {
            ptr: NonNull::new(ptr.cast()).expect("mmap returned non-null"),
            len: self.len,
        })
    }
}

/// One private branch mapping. Dropping it unmaps the region before the backing fd can disappear.
pub struct PrivateCowMapping {
    ptr: NonNull<u8>,
    len: usize,
}

impl PrivateCowMapping {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for PrivateCowMapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

fn page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_branch_writes_do_not_change_source_or_sibling() {
        let page = page_size();
        let mut bytes = vec![0u8; page * 2];
        bytes[page + 7] = 0x42;
        let backing = GuestRamBacking::from_bytes(&bytes).unwrap();
        let mut first = backing.map_private().unwrap();
        let second = backing.map_private().unwrap();
        first.as_mut_slice()[page + 7] = 0x99;
        assert_eq!(first.as_slice()[page + 7], 0x99);
        assert_eq!(second.as_slice()[page + 7], 0x42);
        assert_eq!(bytes[page + 7], 0x42);
    }

    #[test]
    fn non_page_aligned_backing_fails_closed() {
        assert!(matches!(
            GuestRamBacking::from_bytes(&[0; 3]),
            Err(BackingError::InvalidLength)
        ));
    }
}
