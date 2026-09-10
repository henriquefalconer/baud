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
const UFFD_FEATURE_PAGEFAULT_FLAG_WP: u64 = 1 << 0;
const UFFD_FEATURE_MINOR: u64 = 1 << 16;
const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;
const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;
const UFFDIO_REGISTER_MODE_MINOR: u64 = 1 << 2;
const UFFDIO_WRITEPROTECT_MODE_WP: u64 = 1 << 0;
// ioctl capability bits returned by UFFDIO_API, matching linux/userfaultfd.h.
const UFFDIO_REGISTER_IOC: u64 = 1 << 0;
const UFFDIO_WRITEPROTECT_IOC: u64 = 1 << 6;
const UFFDIO_CONTINUE_IOC: u64 = 1 << 7;
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1 << 0;
const UFFD_PAGEFAULT_FLAG_WP: u64 = 1 << 1;
const UFFD_PAGEFAULT_FLAG_MINOR: u64 = 1 << 2;

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
struct Message {
    event: u8,
    reserved1: u8,
    reserved2: u16,
    reserved3: u32,
    flags: u64,
    address: u64,
    reserved4: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub address: u64,
    pub write: bool,
    pub write_protect: bool,
    pub minor: bool,
}

impl Message {
    fn fault(self) -> Result<Fault, Error> {
        if self.event != UFFD_EVENT_PAGEFAULT {
            return Err(Error::UnexpectedEvent(self.event));
        }
        Ok(Fault {
            address: self.address,
            write: self.flags & UFFD_PAGEFAULT_FLAG_WRITE != 0,
            write_protect: self.flags & UFFD_PAGEFAULT_FLAG_WP != 0,
            minor: self.flags & UFFD_PAGEFAULT_FLAG_MINOR != 0,
        })
    }
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
    #[error("userfaultfd rejected required ioctl capability {0:#x}")]
    MissingIoctl(u64),
    #[error("userfaultfd range is not page aligned")]
    UnalignedRange,
    #[error("userfaultfd returned unsupported event {0:#x}")]
    UnexpectedEvent(u8),
    #[error("userfaultfd returned a truncated event")]
    ShortRead,
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
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        if page == 0 || !start.is_multiple_of(page) || len == 0 || !len.is_multiple_of(page) {
            return Err(Error::UnalignedRange);
        }
        let fd =
            unsafe { libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as std::os::fd::RawFd) };
        // Write-protect mode has its own feature bit. Negotiating only MINOR makes the
        // subsequent registration look valid on some kernels but leaves WP page faults
        // unsupported, which turns a claimed CoW region into an unhandled fault path.
        let requested_features = UFFD_FEATURE_MINOR | UFFD_FEATURE_PAGEFAULT_FLAG_WP;
        let mut api = Api {
            api: UFFD_API,
            features: requested_features,
            ..Api::default()
        };
        if unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_API, &mut api) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if api.api != UFFD_API {
            return Err(Error::Api(api.api, UFFD_API));
        }
        if api.features & requested_features != requested_features {
            return Err(Error::MissingFeature(requested_features & !api.features));
        }
        let required_ioctls = UFFDIO_REGISTER_IOC | UFFDIO_WRITEPROTECT_IOC | UFFDIO_CONTINUE_IOC;
        if api.ioctls & required_ioctls != required_ioctls {
            return Err(Error::MissingIoctl(required_ioctls & !api.ioctls));
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

    /// Read one page-fault event from the nonblocking descriptor. `Ok(None)` means there is no
    /// event yet, so a branch worker can drain faults without blocking the vCPU thread.
    pub fn read_fault(&self) -> Result<Option<Fault>, Error> {
        let mut message = Message::default();
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                (&mut message as *mut Message).cast(),
                std::mem::size_of::<Message>(),
            )
        };
        if n == 0 {
            return Ok(None);
        }
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(error.into());
        }
        if n as usize != std::mem::size_of::<Message>() {
            return Err(Error::ShortRead);
        }
        message.fault().map(Some)
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
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        if page == 0
            || !address.is_multiple_of(page)
            || length == 0
            || !length.is_multiple_of(page)
            || !source.is_multiple_of(page)
            || address < self.start
            || length > self.len
            || address - self.start > self.len - length
        {
            return Err(Error::UnalignedRange);
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
    fn requested_features_include_minor_faults_and_write_protection() {
        let requested = UFFD_FEATURE_MINOR | UFFD_FEATURE_PAGEFAULT_FLAG_WP;
        assert_ne!(requested & UFFD_FEATURE_MINOR, 0);
        assert_ne!(requested & UFFD_FEATURE_PAGEFAULT_FLAG_WP, 0);
    }

    #[test]
    fn ioctl_numbers_match_linux_abi() {
        assert_eq!(UFFDIO_API, 0xC018AA3F);
        assert_eq!(UFFDIO_REGISTER, 0xC020AA00);
        assert_eq!(UFFDIO_WRITEPROTECT, 0xC018AA06);
        assert_eq!(UFFDIO_CONTINUE, 0xC028AA07);
    }

    #[test]
    fn pagefault_message_decodes_minor_and_write_protect_flags() {
        let message = Message {
            event: UFFD_EVENT_PAGEFAULT,
            flags: UFFD_PAGEFAULT_FLAG_WRITE | UFFD_PAGEFAULT_FLAG_WP | UFFD_PAGEFAULT_FLAG_MINOR,
            address: 0x4000,
            ..Message::default()
        };
        assert_eq!(
            message.fault().unwrap(),
            Fault { address: 0x4000, write: true, write_protect: true, minor: true }
        );
    }

    #[test]
    fn unsupported_pagefault_event_fails_closed() {
        assert!(matches!(Message { event: 1, ..Message::default() }.fault(), Err(Error::UnexpectedEvent(1))));
    }

    #[test]
    fn unaligned_regions_fail_before_touching_userfaultfd() {
        assert!(matches!(
            CowRegion::open(1, 4096),
            Err(Error::UnalignedRange)
        ));
        assert!(matches!(CowRegion::open(0, 0), Err(Error::UnalignedRange)));
    }
}
