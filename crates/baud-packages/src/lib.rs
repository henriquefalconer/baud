// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-packages — workload specs as Nix building blocks
//
// spec.toml → pinned flake → nix build/copy → closure hash → manifest
//
// Design:
//   - One flake template + substitution; pinned nixpkgs rev in one place
//   - Wraps `nix build` and `nix copy` only
//   - Any nixpkgs-expressible derivation satisfying the guest contract is valid
//   - Closure hash journaled per run; reconstruction requires the same closure
//
// This crate is a pure library (no I/O in the library proper for testability).
// The `build` and `build_in_dir` functions shell out to `nix` when a real build
// is requested.

use serde::{Deserialize, Serialize};
use anyhow::{bail, Result};
use std::path::PathBuf;

mod spec;
mod flake;

pub use spec::{WorkloadSpec, WorkloadPackage};
pub use flake::FlakeTemplate;

// ---------------------------------------------------------------------------
// Pinned nixpkgs revision (the single source of truth)
// ---------------------------------------------------------------------------

/// The nixpkgs revision pinned for all guest builds.
/// Changing this is a deliberate, reviewed edit.
pub const NIXPKGS_REV: &str = "23.11";

// ---------------------------------------------------------------------------
// Build result
// ---------------------------------------------------------------------------

/// Result of building a workload spec via nix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    /// Path to the built guest binary (inside /nix/store or a local output path)
    pub guest_path: PathBuf,
    /// blake3 hash of the closure (inputs + output paths, canonical form)
    pub closure_hash: String,
    /// Whether this was a real nix build or a stub/cached result
    pub is_stub: bool,
}

impl BuildResult {
    /// Verify that the binary is static and no-PIE by inspecting ELF headers.
    ///
    /// Returns `Ok(())` if the binary passes the guest contract, `Err` with a
    /// description of the violation otherwise.
    ///
    /// On macOS (where the cross-built binary is not a local ELF), this is a
    /// no-op if the binary does not exist locally (stub builds).
    pub fn verify_guest_contract(&self) -> Result<()> {
        if self.is_stub {
            // Stub builds do not produce real binaries; contract check is skipped.
            return Ok(());
        }
        if !self.guest_path.exists() {
            bail!("guest binary does not exist: {}", self.guest_path.display());
        }
        // Read ELF header
        let bytes = std::fs::read(&self.guest_path)?;
        if bytes.len() < 64 {
            bail!("guest binary too small to be an ELF");
        }
        // Check ELF magic
        if &bytes[0..4] != b"\x7fELF" {
            bail!("guest binary is not an ELF file");
        }
        // Check ET_DYN (PIE) vs ET_EXEC (static no-PIE)
        // e_type is at offset 16 (2 bytes, little-endian on x86_64)
        let e_type = u16::from_le_bytes([bytes[16], bytes[17]]);
        const ET_EXEC: u16 = 2;
        const ET_DYN: u16 = 3;
        if e_type == ET_DYN {
            bail!(
                "guest binary is PIE (ET_DYN); guest contract requires no-PIE (ET_EXEC): {}",
                self.guest_path.display()
            );
        }
        if e_type != ET_EXEC {
            bail!(
                "guest binary has unexpected ELF type {e_type:#x}; expected ET_EXEC ({ET_EXEC:#x})"
            );
        }
        // Check for dynamic linking: PT_INTERP segment means dynamically linked
        // (Parse program headers; e_phoff at offset 32, e_phentsize at 54, e_phnum at 56)
        let e_phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
        let e_phentsize = u16::from_le_bytes([bytes[54], bytes[55]]) as usize;
        let e_phnum = u16::from_le_bytes([bytes[56], bytes[57]]) as usize;
        for i in 0..e_phnum {
            let off = e_phoff + i * e_phentsize;
            if off + 4 > bytes.len() {
                break;
            }
            let p_type = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            const PT_INTERP: u32 = 3;
            if p_type == PT_INTERP {
                bail!(
                    "guest binary has PT_INTERP segment (dynamically linked); \
                     guest contract requires statically linked musl: {}",
                    self.guest_path.display()
                );
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse and validate a `spec.toml` file.
pub fn lint_spec(toml_str: &str) -> Result<WorkloadSpec> {
    spec::parse_and_lint(toml_str)
}

/// Build a workload spec, returning the closure hash.
///
/// If `nix` is not installed or `dry_run` is true, returns a stub result
/// with a deterministic hash of the spec contents.
///
/// On a real build, this calls `nix build` inside a temp directory with the
/// generated flake, then computes the closure hash via `nix path-info`.
pub fn build(spec: &WorkloadSpec, dry_run: bool) -> Result<BuildResult> {
    if dry_run || !nix_available() {
        return Ok(stub_result(spec));
    }
    build_real(spec)
}

/// Check whether `nix` is available on PATH.
pub fn nix_available() -> bool {
    std::process::Command::new("nix")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Stub build (no nix required)
// ---------------------------------------------------------------------------

fn stub_result(spec: &WorkloadSpec) -> BuildResult {
    // Derive a deterministic closure hash from the spec contents.
    let input = format!(
        "stub:{}:{}:{}:{}",
        spec.workload.name,
        spec.workload.packages.join(","),
        spec.workload.build,
        NIXPKGS_REV,
    );
    let hash = blake3::hash(input.as_bytes());
    BuildResult {
        guest_path: PathBuf::from(format!("/nix/store/stub-{}", spec.workload.name)),
        closure_hash: format!("blake3:{}", hex::encode(hash.as_bytes())),
        is_stub: true,
    }
}

// ---------------------------------------------------------------------------
// Real nix build (nix must be on PATH)
// ---------------------------------------------------------------------------

fn build_real(spec: &WorkloadSpec) -> Result<BuildResult> {
    let dir = tempfile::tempdir()?;
    let flake = FlakeTemplate::generate(spec, NIXPKGS_REV)?;

    // Write flake.nix
    let flake_path = dir.path().join("flake.nix");
    std::fs::write(&flake_path, &flake)?;

    // nix build .#guest -o result
    let output = std::process::Command::new("nix")
        .args(["build", ".#guest", "-o", "result", "--no-link"])
        .current_dir(dir.path())
        .output()?;

    if !output.status.success() {
        bail!(
            "nix build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // nix path-info .#guest → store path
    let path_info = std::process::Command::new("nix")
        .args(["path-info", ".#guest"])
        .current_dir(dir.path())
        .output()?;

    if !path_info.status.success() {
        bail!(
            "nix path-info failed:\n{}",
            String::from_utf8_lossy(&path_info.stderr)
        );
    }

    let store_path = String::from_utf8(path_info.stdout)?.trim().to_string();

    // Compute closure hash: hash of sorted closure paths
    let closure_out = std::process::Command::new("nix")
        .args(["path-info", "--recursive", &store_path])
        .output()?;
    let closure_text = String::from_utf8(closure_out.stdout)?;
    let mut paths: Vec<&str> = closure_text.lines().collect();
    paths.sort();
    let closure_hash = blake3::hash(paths.join("\n").as_bytes());

    Ok(BuildResult {
        guest_path: PathBuf::from(format!("{}/bin/{}", store_path, spec.workload.name)),
        closure_hash: format!("blake3:{}", hex::encode(closure_hash.as_bytes())),
        is_stub: false,
    })
}

// ---------------------------------------------------------------------------
// hex encoding helper (inline, no extra dep)
// ---------------------------------------------------------------------------

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_build_is_reproducible() {
        let toml = r#"
[workload]
name = "parser"
packages = ["stdenv", "musl"]
build = "cc -static -no-pie -o guest parser.c"
"#;
        let spec = lint_spec(toml).unwrap();
        let r1 = build(&spec, true).unwrap();
        let r2 = build(&spec, true).unwrap();
        assert_eq!(r1.closure_hash, r2.closure_hash, "stub build must be reproducible");
    }

    #[test]
    fn stub_build_contract_check_skipped() {
        let toml = r#"
[workload]
name = "hello"
packages = ["hello"]
build = "cp ${hello}/bin/hello $out"
"#;
        let spec = lint_spec(toml).unwrap();
        let r = build(&spec, true).unwrap();
        // Stub: contract check is a no-op
        assert!(r.verify_guest_contract().is_ok());
    }

    #[test]
    fn different_specs_have_different_hashes() {
        let toml1 = r#"
[workload]
name = "parser"
packages = ["stdenv"]
build = "cc -o guest parser.c"
"#;
        let toml2 = r#"
[workload]
name = "consensus-target"
packages = ["rustc"]
build = "cargo build"
"#;
        let r1 = build(&lint_spec(toml1).unwrap(), true).unwrap();
        let r2 = build(&lint_spec(toml2).unwrap(), true).unwrap();
        assert_ne!(r1.closure_hash, r2.closure_hash);
    }

    #[test]
    fn unknown_workload_field_is_error() {
        let toml = r#"
[workload]
name = "hello"
packages = ["hello"]
build = "cp $hello $out"
bogus_field = true
"#;
        assert!(lint_spec(toml).is_err(), "unknown field must be an error");
    }
}
