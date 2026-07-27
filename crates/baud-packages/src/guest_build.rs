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

    /// A *stub* kernel tree: a `Makefile` and a `scripts/kconfig/merge_config.sh` that accept
    /// exactly the invocations `kernel_build::build_bzimage` makes, append each one to
    /// `<tree>/build-steps.log`, and (for `bzImage`) produce the
    /// `arch/x86/boot/bzImage` artifact the real builder then requires. Nothing here compiles
    /// anything -- the point is to exercise *our* orchestration (step order, env pinning,
    /// merge_config arguments, artifact plumbing, initramfs assembly, hashing) in about a second,
    /// with no `gcc-13`, no `musl-gcc` and no 1.8 GB kernel checkout. Real-toolchain coverage stays
    /// in the `#[ignore]`d `image_build_is_reproducible` (`drive/pkg/pkg-image-build.sh`); this is the
    /// fast in-repo half that runs on every `cargo test`.
    fn write_stub_kernel_tree(root: &Path) {
        // `$@` is the target; the env echo pins deterministic_build_env()'s variables so a
        // regression that stops exporting them shows up in the log rather than silently.
        fs::write(
            root.join("Makefile"),
            "mrproper allnoconfig olddefconfig:\n\
             \t@echo \"make $@\" >> $(CURDIR)/build-steps.log\n\
             \t@touch $(CURDIR)/.config\n\
             \n\
             bzImage:\n\
             \t@echo \"make $@ ts=$$KBUILD_BUILD_TIMESTAMP user=$$KBUILD_BUILD_USER \
             host=$$KBUILD_BUILD_HOST epoch=$$SOURCE_DATE_EPOCH\" >> $(CURDIR)/build-steps.log\n\
             \t@mkdir -p $(CURDIR)/arch/x86/boot\n\
             \t@printf 'stub-bzImage-bytes' > $(CURDIR)/arch/x86/boot/bzImage\n",
        )
        .unwrap();

        fs::create_dir_all(root.join("scripts/kconfig")).unwrap();
        let merge_script = root.join("scripts/kconfig/merge_config.sh");
        fs::write(
            &merge_script,
            "#!/usr/bin/env bash\n\
             set -eu\n\
             echo \"merge_config.sh $*\" >> \"$PWD/build-steps.log\"\n\
             # A real merge_config.sh reads both files; failing here if either is missing keeps\n\
             # the stub honest about build_bzimage's ordering contract.\n\
             test -f .config\n\
             cat \"$3\" >> .config\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&merge_script, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn gunzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes).read_to_end(&mut out).unwrap();
        out
    }

    /// Fast, toolchain-free smoke test of the whole in-repo `spec -> image` chain against the stub
    /// tree above: `build_guest_image` -> `build_bzimage` (`make` + `merge_config.sh`) ->
    /// `build_reproducible_initramfs` (the real `flate2` cpio.gz path) -> §4.5's identity hashes.
    #[test]
    fn build_guest_image_drives_the_real_pipeline_against_a_stub_kernel_tree() {
        let kernel_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        write_stub_kernel_tree(kernel_dir.path());

        let fragment = work.path().join("minimal.config");
        fs::write(&fragment, "CONFIG_BAUD_STUB=y\n").unwrap();
        let init_src = work.path().join("init");
        fs::write(&init_src, b"#!stub-init-binary").unwrap();
        let helper_src = work.path().join("helper");
        fs::write(&helper_src, b"#!stub-helper-binary").unwrap();

        let entries = [
            InitramfsFileEntry {
                archive_path: "init".to_string(),
                mode: 0o755,
                source_path: init_src,
            },
            InitramfsFileEntry {
                archive_path: "helper".to_string(),
                mode: 0o755,
                source_path: helper_src,
            },
        ];
        let output_dir = work.path().join("out");
        let result = build_guest_image(&GuestImageBuildConfig {
            kernel: KernelBuildConfig {
                kernel_src: kernel_dir.path(),
                config_fragment: &fragment,
                cc: "stub-cc",
                jobs: Some(1),
            },
            initramfs_entries: &entries,
            output_dir: &output_dir,
        })
        .expect("stub-tree guest image build failed");

        // 1. The kernel steps ran, in spec §4.5's order, and nothing else ran.
        let log = fs::read_to_string(kernel_dir.path().join("build-steps.log")).unwrap();
        let steps: Vec<&str> = log.lines().collect();
        assert_eq!(steps.len(), 5, "unexpected step count in:\n{log}");
        assert_eq!(steps[0], "make mrproper");
        assert_eq!(steps[1], "make allnoconfig");
        assert!(
            steps[2].starts_with("merge_config.sh -m .config /") && steps[2].ends_with("minimal.config"),
            "merge_config.sh must be handed `-m .config <canonicalized fragment>`, got: {}",
            steps[2]
        );
        assert_eq!(steps[3], "make olddefconfig");
        // deterministic_build_env() must reach the compiler's environment, or two builds of the
        // same source stop being byte-identical (the `image_build_is_reproducible` failure mode).
        assert_eq!(
            steps[4],
            "make bzImage ts=@0 user=baud host=baud epoch=0",
            "every build-identity variable deterministic_build_env() pins must be exported"
        );

        // 2. The built bzImage was copied out verbatim.
        assert_eq!(result.bzimage_path, output_dir.join("bzImage"));
        assert_eq!(fs::read(&result.bzimage_path).unwrap(), b"stub-bzImage-bytes");

        // 3. A real initramfs came out of the real flate2 path -- gzip magic on disk, and both
        //    entries present in the decompressed newc cpio stream.
        assert_eq!(result.initramfs_path, output_dir.join("initramfs.cpio.gz"));
        let gz = fs::read(&result.initramfs_path).unwrap();
        assert_eq!(&gz[..2], &[0x1f, 0x8b], "initramfs must be gzip-compressed");
        let cpio = gunzip(&gz);
        for needle in [
            &b"init"[..],
            b"helper",
            b"#!stub-init-binary",
            b"#!stub-helper-binary",
            b"TRAILER!!!",
        ] {
            assert!(
                cpio.windows(needle.len()).any(|w| w == needle),
                "decompressed cpio is missing {:?}",
                String::from_utf8_lossy(needle)
            );
        }

        // 4. All three §4.5 identities are well-formed lowercase sha256 hex.
        for (label, hash) in [
            ("bzimage_sha256", &result.bzimage_sha256),
            ("initramfs_sha256", &result.initramfs_sha256),
            ("image_hash", &result.image_hash),
        ] {
            assert_eq!(hash.len(), 64, "{label} must be 64 hex chars, got {hash:?}");
            assert!(
                hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{label} must be lowercase hex, got {hash:?}"
            );
        }
        assert_eq!(
            hash_image(b"stub-bzImage-bytes", &gz).2,
            result.image_hash,
            "image_hash must be sha256(bzImage ‖ initramfs.gz) over exactly the emitted artifacts"
        );
    }
}
