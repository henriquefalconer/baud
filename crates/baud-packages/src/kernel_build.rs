// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Automates the by-hand guest-kernel build recipe documented in
// `crates/baud-multiverse/tests/fixtures/linux-guest/BUILD.md`'s "Regenerating the kernel"
// section (todo.md §4.5 / §14 next-actions item 1's "next concrete step": "a plain from-source
// `make bzImage` could reuse the kernel source tree already checked out at
// `~/wsl-kernel-src/src`"): `mrproper -> allnoconfig -> merge_config.sh -> olddefconfig ->
// bzImage`, driven from Rust instead of copy-pasted shell.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Where to build a guest kernel from, and with what config.
pub struct KernelBuildConfig<'a> {
    /// A **writable, disposable** copy of a Linux kernel source tree -- BUILD.md's own warning
    /// applies here too: never build in the shared tree at `~/wsl-kernel-src/src` used for the
    /// enforced-`kvm_intel` module work (CLAUDE.md); that tree carries host-module build
    /// artifacts and applied enforcement patches unrelated to a guest kernel. Copy it first.
    pub kernel_src: &'a Path,
    /// A Kconfig fragment merged on top of `allnoconfig` (spec §4.1's required/disabled list --
    /// e.g. `tests/fixtures/linux-guest/minimal.config`).
    pub config_fragment: &'a Path,
    /// The compiler to build with. CLAUDE.md: must match this dev host's kernel-build gcc major
    /// version (`gcc-13`) or struct-ABI details silently diverge.
    pub cc: &'a str,
    /// Parallel build jobs (`make -jN`). `None` uses the host's available parallelism.
    pub jobs: Option<usize>,
}

/// Env vars that pin every build-timestamp/build-identity string Kbuild embeds in the compiled
/// kernel (`UTS_VERSION`'s `#N SMP PREEMPT ... <date>`, the build user/host strings). Left unset,
/// two builds of byte-identical source + config still produce a *different* `bzImage`, because
/// the real wall-clock build time and `whoami`/`hostname` get baked into the binary -- exactly
/// the hidden-nondeterminism class spec §4.5's `image_build_is_reproducible` test exists to catch.
fn deterministic_build_env() -> [(&'static str, &'static str); 4] {
    [
        ("KBUILD_BUILD_TIMESTAMP", "@0"),
        ("KBUILD_BUILD_USER", "baud"),
        ("KBUILD_BUILD_HOST", "baud"),
        ("SOURCE_DATE_EPOCH", "0"),
    ]
}

fn run_make(kernel_src: &Path, cc: &str, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("make");
    cmd.arg(format!("CC={cc}"));
    cmd.args(args);
    cmd.current_dir(kernel_src);
    for (key, value) in deterministic_build_env() {
        cmd.env(key, value);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn `make CC={cc} {}` in {}", args.join(" "), kernel_src.display()))?;
    if !status.success() {
        bail!("`make CC={cc} {}` failed with {status} in {}", args.join(" "), kernel_src.display());
    }
    Ok(())
}

/// Run the full `mrproper -> allnoconfig -> merge_config.sh -> olddefconfig -> bzImage` pipeline
/// against `cfg.kernel_src`, returning the path to the built `bzImage`
/// (`<kernel_src>/arch/x86/boot/bzImage`).
pub fn build_bzimage(cfg: &KernelBuildConfig) -> Result<PathBuf> {
    if !cfg.kernel_src.join("Makefile").exists() {
        bail!(
            "kernel_src {} does not look like a kernel source tree (no Makefile)",
            cfg.kernel_src.display()
        );
    }
    let config_fragment = cfg.config_fragment.canonicalize().with_context(|| {
        format!("config fragment {} not found", cfg.config_fragment.display())
    })?;

    run_make(cfg.kernel_src, cfg.cc, &["mrproper"])?;
    run_make(cfg.kernel_src, cfg.cc, &["allnoconfig"])?;

    let merge_script = cfg.kernel_src.join("scripts/kconfig/merge_config.sh");
    if !merge_script.exists() {
        bail!("merge_config.sh not found at {}", merge_script.display());
    }
    let status = Command::new("bash")
        .arg(&merge_script)
        .arg("-m")
        .arg(".config")
        .arg(&config_fragment)
        .current_dir(cfg.kernel_src)
        .status()
        .context("failed to spawn merge_config.sh")?;
    if !status.success() {
        bail!("merge_config.sh failed with {status}");
    }

    run_make(cfg.kernel_src, cfg.cc, &["olddefconfig"])?;

    let jobs = cfg
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    let jobs_flag = format!("-j{jobs}");
    run_make(cfg.kernel_src, cfg.cc, &[jobs_flag.as_str(), "bzImage"])?;

    let bzimage = cfg.kernel_src.join("arch/x86/boot/bzImage");
    if !bzimage.exists() {
        bail!(
            "build reported success but {} does not exist -- unexpected make output layout",
            bzimage.display()
        );
    }
    Ok(bzimage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_directory_that_is_not_a_kernel_tree() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = KernelBuildConfig {
            kernel_src: dir.path(),
            config_fragment: dir.path(),
            cc: "gcc-13",
            jobs: Some(1),
        };
        let err = build_bzimage(&cfg).unwrap_err();
        assert!(err.to_string().contains("does not look like a kernel source tree"), "{err}");
    }

    /// Real-hardware reproducibility test (todo.md §4.5's spec-named `image_build_is_reproducible`
    /// test): builds the *same* kernel source + config fragment twice, into two independent
    /// scratch copies, and asserts the resulting `bzImage` bytes are byte-identical. Requires a
    /// real kernel source tree (`BAUD_KERNEL_SRC`, default `~/wsl-kernel-src/src` per CLAUDE.md)
    /// and `gcc-13`, and takes several minutes (two full kernel builds) -- gated behind
    /// `#[ignore]` and driven by `drive/pkg-image-build.sh`, the same opt-in convention as the
    /// enforced-regime tests (`drive/h3-enforced-rdtsc.sh` etc).
    #[test]
    #[ignore]
    fn image_build_is_reproducible() {
        let home = std::env::var("HOME").unwrap_or_default();
        let kernel_src_path = std::env::var("BAUD_KERNEL_SRC")
            .unwrap_or_else(|_| format!("{home}/wsl-kernel-src/src"));
        let kernel_src = PathBuf::from(&kernel_src_path);
        if !kernel_src.join("Makefile").exists() {
            eprintln!(
                "Skipping image_build_is_reproducible: no kernel source tree at {kernel_src_path} \
                 (set BAUD_KERNEL_SRC to override)"
            );
            return;
        }
        if Command::new("gcc-13").arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
            eprintln!("Skipping image_build_is_reproducible: gcc-13 not found on PATH");
            return;
        }

        let config_fragment = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/linux-guest/minimal.config");

        let scratch_a = tempfile::tempdir().unwrap();
        let scratch_b = tempfile::tempdir().unwrap();
        for scratch in [scratch_a.path(), scratch_b.path()] {
            let status = Command::new("cp")
                .arg("-a")
                .arg(format!("{}/.", kernel_src.display()))
                .arg(scratch)
                .status()
                .expect("failed to spawn cp -a");
            assert!(status.success(), "cp -a {} -> {} failed", kernel_src.display(), scratch.display());
        }

        let bz_a = build_bzimage(&KernelBuildConfig {
            kernel_src: scratch_a.path(),
            config_fragment: &config_fragment,
            cc: "gcc-13",
            jobs: None,
        })
        .expect("first build failed");
        let bz_b = build_bzimage(&KernelBuildConfig {
            kernel_src: scratch_b.path(),
            config_fragment: &config_fragment,
            cc: "gcc-13",
            jobs: None,
        })
        .expect("second build failed");

        let bytes_a = std::fs::read(bz_a).unwrap();
        let bytes_b = std::fs::read(bz_b).unwrap();
        assert_eq!(
            bytes_a, bytes_b,
            "two builds of the same kernel source + config fragment must be byte-identical -- a \
             mismatch here means a build-timestamp/user/host leak deterministic_build_env() \
             doesn't yet pin, or genuine build nondeterminism upstream"
        );
    }
}
