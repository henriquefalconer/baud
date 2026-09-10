// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Small raw userfaultfd wrapper. We keep this here instead of depending on the bindgen-based
// `userfaultfd` crate because the snapshot crate must cross-check on hosts without libclang.
// The layout is copied from linux/userfaultfd.h and is checked by the kernel through ioctl.

#![cfg(target_os = "linux")]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const UFFD_API: u64 = 0xAA;
const UFFD_FEATURE_MINOR: u64 = 1 << 16;
const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;
const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;
const UFFDIO_REGISTER_MODE_MINOR: u64 = 1 << 2;
const UFFDIO_WRITEPROTECT_MODE_WP: u64 = 1 << 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Range {
    start: u64,
    len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Api {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Register {
    range: Range,
    mode: u64,
    ioctls: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WriteProtect {
    range: Range,
    mode: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Continue {
    range: Range,
    mode: u64,
    copy: u64,
    wp_copy: u64,
}

const fn ioctl(dir: u64, nr: u64, size: u64) -> libc::c_ulong {
    ((dir << 30) | (size << 16) | (0xAA << 8) | nr) as libc::c_ulong
}
const fn iowr<T>(nr: u64) -> libc::c_ulong {
    ioctl(3, nr, std::mem::size_of::<T>() as u64)
}

const UFFDIO_API: libc::c_ulong = iowr::<Api>(0x3f);
const UFFDIO_REGISTER: libc::c_ulong = iowr::<Register>(0x00);
const UFFDIO_WRITEPROTECT: libc::c_ulong = iowr::<WriteProtect>(0x06);
const UFFDIO_CONTINUE: libc::c_ulong = iowr::<Continue>(0x07);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("userfaultfd is unavailable: {0}")]
    Io(#[from] io::Error),
    #[error("userfaultfd API negotiation returned {0:#x}, expected {1:#x}")]
    Api(u64, u64),
    #[error("userfaultfd rejected requested feature {0:#x}")]
    MissingFeature(u64),
}

/// A registered range that can be write-protected and populated from a shared memfd.
/// The caller owns the mapping and must keep it alive until this object is dropped.
pub struct CowRegion {
    fd: OwnedFd,
    start: u64,
    len: u64,
}

impl CowRegion {
    /// Open a nonblocking userfaultfd and negotiate minor-fault plus write-protect support.
    pub fn open(start: u64, len: u64) -> Result<Self, Error> {
        let fd =
            unsafe { libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as std::os::fd::RawFd) };
        let mut api = Api {
            api: UFFD_API,
            features: UFFD_FEATURE_MINOR,
            ..Api::default()
        };
        if unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_API, &mut api) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if api.api != UFFD_API {
            return Err(Error::Api(api.api, UFFD_API));
        }
        if api.features & UFFD_FEATURE_MINOR == 0 {
            return Err(Error::MissingFeature(UFFD_FEATURE_MINOR));
        }
        let mut registration = Register {
            range: Range { start, len },
            mode: UFFDIO_REGISTER_MODE_MINOR | UFFDIO_REGISTER_MODE_WP,
            ..Register::default()
        };
        if unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_REGISTER, &mut registration) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self { fd, start, len })
    }

    pub fn write_protect(&self, enabled: bool) -> Result<(), Error> {
        let mut request = WriteProtect {
            range: Range {
                start: self.start,
                len: self.len,
            },
            mode: if enabled {
                UFFDIO_WRITEPROTECT_MODE_WP
            } else {
                0
            },
        };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), UFFDIO_WRITEPROTECT, &mut request) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// Resolve a minor fault by continuing from the shared backing mapping at `source`.
    pub fn continue_from(&self, address: u64, length: u64, source: u64) -> Result<(), Error> {
        if address < self.start || length > self.len || address - self.start > self.len - length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "UFFD range outside registered region",
            )
            .into());
        }
        let mut request = Continue {
            range: Range {
                start: address,
                len: length,
            },
            copy: source,
            ..Continue::default()
        };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), UFFDIO_CONTINUE, &mut request) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ioctl_numbers_match_linux_abi() {
        assert_eq!(UFFDIO_API, 0xC018AA3F);
        assert_eq!(UFFDIO_REGISTER, 0xC020AA00);
        assert_eq!(UFFDIO_WRITEPROTECT, 0xC018AA06);
        assert_eq!(UFFDIO_CONTINUE, 0xC028AA07);
    }
}
