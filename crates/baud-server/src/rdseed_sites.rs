// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// RdseedRewriteReport -> boot wiring (todo.md §14): `baud image rewrite-rdseed` (`baud-cli`'s
// `ImageAction::RewriteRdseed`) writes a `<output>.rdseed-sites.json` sidecar next to the patched
// guest image. This module is the other half — every real `/run/kvm*` boot looks up that sidecar
// for the `kernel_path` it was handed and, if present, threads its sites into
// `Multiverse::boot_with_rdseed_sites` instead of the empty table `Multiverse::boot` always passes.
//
// A missing sidecar (the common case — most guests, e.g. an image with no `rdseed` at all, never
// get one written, or a caller boots a hand-built fixture that predates this mechanism entirely)
// is not an error: it means exactly what `Multiverse::boot`'s empty table already means, "no site
// is ever served a value" (`boot_with_rdseed_sites`'s own doc: "always safe, never a silent
// serve-a-guess-for-every-#UD"). A sidecar that *does* exist but fails to parse is different — that
// is image corruption or a build-pipeline bug, and boot fails loud rather than silently booting
// with no enforcement, matching this codebase's determinism-hole philosophy (§3.6).

use std::path::{Path, PathBuf};

/// Where `baud image rewrite-rdseed` writes (and this module reads) the rewrite-site sidecar for a
/// given guest image path — same convention on both sides, so no separate manifest/DB lookup is
/// needed to find it.
pub fn sidecar_path(kernel_path: &Path) -> PathBuf {
    let mut name = kernel_path.as_os_str().to_owned();
    name.push(".rdseed-sites.json");
    PathBuf::from(name)
}

/// Load `kernel_path`'s rdseed-sites sidecar (if any) and convert it into the
/// `(guest address, EnforcedRdseedSite)` table `Multiverse::boot_with_rdseed_sites` wants. Returns
/// an empty table — identical to `Multiverse::boot`'s own default — when no sidecar exists.
pub fn load_rdseed_sites(
    kernel_path: &Path,
) -> Result<Vec<(u64, baud_vcpu::EnforcedRdseedSite)>, String> {
    let path = sidecar_path(kernel_path);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("failed to read rdseed-sites sidecar '{}': {e}", path.display())),
    };
    let report: baud_packages::RdseedRewriteReport = serde_json::from_slice(&bytes)
        .map_err(|e| format!("rdseed-sites sidecar '{}' is not valid JSON: {e}", path.display()))?;
    Ok(report
        .sites
        .into_iter()
        .map(|site| {
            (site.address, baud_vcpu::EnforcedRdseedSite { gpr_index: site.gpr_index, length: site.length })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sidecar_yields_an_empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let kernel_path = dir.path().join("bzImage");
        std::fs::write(&kernel_path, b"not a real kernel, just needs to exist").unwrap();

        assert_eq!(load_rdseed_sites(&kernel_path).unwrap(), Vec::new());
    }

    #[test]
    fn sidecar_sites_convert_to_enforced_rdseed_sites() {
        let dir = tempfile::tempdir().unwrap();
        let kernel_path = dir.path().join("bzImage");
        std::fs::write(&kernel_path, b"not a real kernel, just needs to exist").unwrap();
        std::fs::write(
            sidecar_path(&kernel_path),
            r#"{"sites":[
                {"section":".text","file_offset":16,"address":2097159,"length":3,"gpr_index":0},
                {"section":".text","file_offset":32,"address":2097175,"length":4,"gpr_index":8}
            ]}"#,
        )
        .unwrap();

        let sites = load_rdseed_sites(&kernel_path).unwrap();
        assert_eq!(
            sites,
            vec![
                (2097159, baud_vcpu::EnforcedRdseedSite { gpr_index: 0, length: 3 }),
                (2097175, baud_vcpu::EnforcedRdseedSite { gpr_index: 8, length: 4 }),
            ]
        );
    }

    #[test]
    fn malformed_sidecar_is_a_loud_error_not_a_silent_empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let kernel_path = dir.path().join("bzImage");
        std::fs::write(&kernel_path, b"not a real kernel, just needs to exist").unwrap();
        std::fs::write(sidecar_path(&kernel_path), b"not json").unwrap();

        let err = load_rdseed_sites(&kernel_path).unwrap_err();
        assert!(err.contains("not valid JSON"), "error should name the real cause: {err}");
    }
}
