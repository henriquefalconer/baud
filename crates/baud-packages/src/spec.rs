// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// spec.toml parsing for baud-packages.

use serde::{Deserialize, Serialize};
use anyhow::{bail, Result};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The `[workload]` section of a `spec.toml` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadPackage {
    /// Workload name (used for the output binary and flake attribute)
    pub name: String,
    /// Nix package names to include (e.g. ["stdenv", "musl"])
    pub packages: Vec<String>,
    /// Build command (template, filled in by the flake generator)
    pub build: String,
}

/// A parsed and validated `spec.toml` document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadSpec {
    pub workload: WorkloadPackage,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

pub fn parse_and_lint(toml_str: &str) -> Result<WorkloadSpec> {
    let spec: WorkloadSpec = toml::from_str(toml_str)
        .map_err(|e| anyhow::anyhow!("spec.toml parse error: {e}"))?;

    if spec.workload.name.is_empty() {
        bail!("workload.name must not be empty");
    }
    if spec.workload.build.is_empty() {
        bail!("workload.build must not be empty");
    }

    // Name must be a valid identifier (alphanumeric + hyphens)
    if !spec
        .workload
        .name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "workload.name '{}' must contain only alphanumeric characters, hyphens, or underscores",
            spec.workload.name
        );
    }

    Ok(spec)
}
