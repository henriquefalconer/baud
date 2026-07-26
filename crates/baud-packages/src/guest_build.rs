// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// End-to-end reproducible guest-image build (todo.md §4.5 / §14 next-actions item 1: "no `baud
// image build` command exists yet ... the reproducible-initramfs and kernel-build pieces are not
// yet composed into one end-to-end 'spec.toml in, guest image out' pipeline"). Composes the two
// already-built, already-tested pieces -- `kernel_build::build_bzimage` and
// `initramfs::build_reproducible_initramfs` -- into one call that produces a `bzImage` +
// `initramfs.cpio.gz` pair plus spec §4.5's image identity: `sha256(bzImage ‖ initramfs.gz)`.
//
// Inputs are host-local filesystem paths, not inline content transferred over HTTP: a kernel
// source tree and an initramfs entry's file contents are exactly the kind of large, host-side
// build inputs `KernelBuildConfig` already takes as paths (unlike `/image/rewrite-rdseed`'s
// small, in-memory ELF payload) -- `baud-server`'s `/image/build` route resolves these paths on
// the server host, the same convention `/host/probe` already uses for host-local operations.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::initramfs::{build_reproducible_initramfs, InitramfsEntry};
use crate::kernel_build::{build_bzimage, KernelBuildConfig};

/// One file to place into the initramfs, sourced from an existing file already on disk.
pub struct InitramfsFileEntry {
    /// Path inside the initramfs archive (e.g. `"init"`) -- see `InitramfsEntry::path`.
    pub archive_path: String,
    /// Unix permission bits (e.g. `0o755`) -- see `InitramfsEntry::mode`.
    pub mode: u32,
    /// Where to read the file's contents from on this host.
    pub source_path: PathBuf,
}

/// Everything needed to build one guest image: a kernel (§4.1/§4.2) plus an initramfs (§4.3).
pub struct GuestImageBuildConfig<'a> {
    pub kernel: KernelBuildConfig<'a>,
    pub initramfs_entries: &'a [InitramfsFileEntry],
    /// Directory the built `bzImage` and `initramfs.cpio.gz` are copied into (created if absent).
    pub output_dir: &'a Path,
}

#[derive(Debug)]
pub struct GuestImageBuildResult {
    pub bzimage_path: PathBuf,
    pub initramfs_path: PathBuf,
    pub bzimage_sha256: String,
    pub initramfs_sha256: String,
    /// Spec §4.5's image identity: `sha256(bzImage ‖ initramfs.gz)`.
    pub image_hash: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Combine already-built bzImage + initramfs bytes into the three identity hashes spec §4.5
/// names. Pure and side-effect-free (no filesystem/process I/O) so it is unit-testable without a
/// real kernel build -- `build_guest_image` below is the real-I/O caller.
pub fn hash_image(bzimage_bytes: &[u8], initramfs_bytes: &[u8]) -> (String, String, String) {
    let bzimage_sha256 = sha256_hex(bzimage_bytes);
    let initramfs_sha256 = sha256_hex(initramfs_bytes);
    let mut combined = Vec::with_capacity(bzimage_bytes.len() + initramfs_bytes.len());
    combined.extend_from_slice(bzimage_bytes);
    combined.extend_from_slice(initramfs_bytes);
    let image_hash = sha256_hex(&combined);
    (bzimage_sha256, initramfs_sha256, image_hash)
}

/// Build a real kernel (via `build_bzimage`) plus a reproducible initramfs (via
/// `build_reproducible_initramfs`) and write both into `cfg.output_dir`. Real I/O: shells out to
/// `make`/`merge_config.sh` and reads every `initramfs_entries` source file from disk.
pub fn build_guest_image(cfg: &GuestImageBuildConfig) -> Result<GuestImageBuildResult> {
    let bzimage_src = build_bzimage(&cfg.kernel)?;
    let bzimage_bytes = fs::read(&bzimage_src)
        .with_context(|| format!("failed to read built bzImage at {}", bzimage_src.display()))?;

    let mut entries = Vec::with_capacity(cfg.initramfs_entries.len());
    for entry in cfg.initramfs_entries {
        let contents = fs::read(&entry.source_path).with_context(|| {
            format!(
                "failed to read initramfs entry source '{}' (archive path '{}')",
                entry.source_path.display(),
                entry.archive_path
            )
        })?;
        entries.push(InitramfsEntry::regular(entry.archive_path.clone(), entry.mode, contents));
    }
    let initramfs_bytes = build_reproducible_initramfs(&entries)?;

    fs::create_dir_all(cfg.output_dir)
        .with_context(|| format!("failed to create output dir {}", cfg.output_dir.display()))?;
    let bzimage_path = cfg.output_dir.join("bzImage");
    let initramfs_path = cfg.output_dir.join("initramfs.cpio.gz");
    fs::write(&bzimage_path, &bzimage_bytes)
        .with_context(|| format!("failed to write {}", bzimage_path.display()))?;
    fs::write(&initramfs_path, &initramfs_bytes)
        .with_context(|| format!("failed to write {}", initramfs_path.display()))?;

    let (bzimage_sha256, initramfs_sha256, image_hash) =
        hash_image(&bzimage_bytes, &initramfs_bytes);

    Ok(GuestImageBuildResult {
        bzimage_path,
        initramfs_path,
        bzimage_sha256,
        initramfs_sha256,
        image_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_image_is_reproducible_and_content_sensitive() {
        let (b1, i1, h1) = hash_image(b"kernel-bytes", b"initramfs-bytes");
        let (b2, i2, h2) = hash_image(b"kernel-bytes", b"initramfs-bytes");
        assert_eq!(b1, b2);
        assert_eq!(i1, i2);
        assert_eq!(h1, h2, "identical inputs must yield an identical image_hash");

        let (_, _, h3) = hash_image(b"kernel-bytes-changed", b"initramfs-bytes");
        assert_ne!(h1, h3, "changing the kernel bytes must change the image hash");

        let (_, _, h4) = hash_image(b"kernel-bytes", b"initramfs-bytes-changed");
        assert_ne!(h1, h4, "changing the initramfs bytes must change the image hash");
    }

    #[test]
    fn hash_image_order_matters() {
        // sha256(A||B) != sha256(B||A) in general -- guards a concatenation-order swap from
        // silently producing a hash that still looks valid.
        let (_, _, ab) = hash_image(b"AAAA", b"BBBB");
        let (_, _, ba) = hash_image(b"BBBB", b"AAAA");
        assert_ne!(ab, ba);
    }

    #[test]
    fn hash_image_matches_independent_sha256() {
        // Pin the exact algorithm/encoding spec §4.5 names (sha256, lowercase hex), not just
        // internal self-consistency -- this is `sha256sum` on the string "hello".
        let (bzimage_sha256, _, _) = hash_image(b"hello", b"");
        assert_eq!(
            bzimage_sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn build_guest_image_fails_fast_on_a_non_kernel_tree_before_touching_initramfs() {
        let dir = tempfile::tempdir().unwrap();
        let kernel_dir = tempfile::tempdir().unwrap();
        let entries = [InitramfsFileEntry {
            archive_path: "init".to_string(),
            mode: 0o755,
            // Deliberately missing -- if build_guest_image reached initramfs assembly before
            // failing on the (not a kernel tree) kernel_src, this would change the error message
            // and this test would catch that ordering regression.
            source_path: dir.path().join("does-not-exist"),
        }];
        let cfg = GuestImageBuildConfig {
            kernel: KernelBuildConfig {
                kernel_src: kernel_dir.path(),
                config_fragment: kernel_dir.path(),
                cc: "gcc-13",
                jobs: Some(1),
            },
            initramfs_entries: &entries,
            output_dir: dir.path(),
        };
        let err = build_guest_image(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("does not look like a kernel source tree"),
            "{err}"
        );
    }
}
