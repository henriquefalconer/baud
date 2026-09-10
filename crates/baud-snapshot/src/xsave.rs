// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use kvm_bindings::{kvm_xsave, Xsave};
use kvm_ioctls::{Cap, VmFd};

#[derive(Debug, thiserror::Error)]
pub enum XsaveError {
    #[error("KVM XSAVE2 capability query failed: {0}")]
    Capability(kvm_ioctls::Error),
    #[error("KVM XSAVE2 is unavailable")]
    Unsupported,
    #[error("invalid XSAVE2 buffer size {0}")]
    InvalidSize(usize),
    #[error("XSAVE2 buffer allocation failed: {0}")]
    Allocation(vmm_sys_util::fam::Error),
}

pub fn size(vm: &VmFd) -> Result<usize, XsaveError> {
    let value = vm.check_extension_int(Cap::Xsave2);
    if value < 0 {
        return Err(XsaveError::Capability(kvm_ioctls::Error::last()));
    }
    if value == 0 {
        return Err(XsaveError::Unsupported);
    }
    extra_words(value as usize)?;
    Ok(value as usize)
}

fn extra_words(bytes: usize) -> Result<usize, XsaveError> {
    if !(std::mem::size_of::<kvm_xsave>()..=1024 * 1024).contains(&bytes) {
        return Err(XsaveError::InvalidSize(bytes));
    }
    Ok((bytes - std::mem::size_of::<kvm_xsave>()).div_ceil(4))
}

pub fn allocate(bytes: usize) -> Result<Xsave, XsaveError> {
    Xsave::new(extra_words(bytes)?).map_err(XsaveError::Allocation)
}

pub fn encode(buffer: &Xsave, bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes);
    for word in buffer
        .as_fam_struct_ref()
        .xsave
        .region
        .iter()
        .chain(buffer.as_slice())
    {
        out.extend_from_slice(&word.to_ne_bytes());
    }
    out.truncate(bytes);
    out
}

pub fn decode(bytes: &[u8]) -> Result<Xsave, XsaveError> {
    let mut buffer = allocate(bytes.len())?;
    let mut words = bytes.chunks(4).map(|chunk| {
        let mut word = [0; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        u32::from_ne_bytes(word)
    });
    // SAFETY: only the fixed XSAVE region is changed, never the FAM length field.
    for word in &mut unsafe { buffer.as_mut_fam_struct() }.xsave.region {
        *word = words.next().expect("validated XSAVE base size");
    }
    for word in buffer.as_mut_slice() {
        *word = words.next().expect("allocated exact extended word count");
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_sizes() {
        for size in [0, 4095, 1024 * 1024 + 1] {
            assert!(allocate(size).is_err());
        }
        assert_eq!(extra_words(4096).unwrap(), 0);
        assert_eq!(extra_words(4097).unwrap(), 1);
        assert_eq!(extra_words(8192).unwrap(), 1024);
    }

    #[test]
    fn preserves_extended_bytes_without_serializing_fam_metadata() {
        for size in [4096, 4097, 8192] {
            let bytes: Vec<u8> = (0..size).map(|i| (i * 31 + i / 256) as u8).collect();
            let buffer = decode(&bytes).unwrap();
            assert_eq!(buffer.as_slice().len(), extra_words(size).unwrap());
            assert_eq!(encode(&buffer, size), bytes);
        }
    }
}
