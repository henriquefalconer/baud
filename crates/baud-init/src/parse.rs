// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// YAML parsing for baud-init spec documents.
//
// Five directive kinds: nix, files, env, nodes, adapters.
// Unknown directives are hard errors.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use anyhow::{bail, Result};
use crate::adapter::{parse_adapters, Adapter};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A file fixture to write to the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesEntry {
    pub path: String,
    pub content: String,
}

/// A node in the workload topology.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSpec {
    pub name: String,
    pub argv: Vec<String>,
    pub adapters: Adapter,
}

/// The parsed and linted spec document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecDoc {
    /// Nix flake reference for the guest closure
    pub nix: String,
    /// Files (fixtures) to write to the sandbox
    pub files: Vec<FilesEntry>,
    /// Environment variables for guests
    pub env: HashMap<String, String>,
    /// Workload topology: list of nodes
    pub nodes: Vec<NodeSpec>,
    /// Top-level adapter declarations (apply to all nodes unless overridden per-node).
    /// Parsed and validated; not silently discarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapters: Option<Adapter>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// The set of valid directive keys.
const VALID_DIRECTIVES: &[&str] = &["nix", "files", "env", "nodes", "adapters"];

pub fn parse_and_lint(yaml: &str) -> Result<SpecDoc> {
    let v: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|e| anyhow::anyhow!("YAML parse error: {e}"))?;
    lint_value(v)
}

pub fn lint_value(v: serde_yaml::Value) -> Result<SpecDoc> {
    let m = match &v {
        serde_yaml::Value::Mapping(m) => m,
        _ => bail!("spec document must be a YAML mapping at the top level"),
    };

    // Check for unknown directives (hard error)
    for (k, _) in m {
        let key = k.as_str().ok_or_else(|| anyhow::anyhow!("directive key must be a string"))?;
        if !VALID_DIRECTIVES.contains(&key) {
            bail!(
                "unknown directive '{key}'; valid directives: {}",
                VALID_DIRECTIVES.join(", ")
            );
        }
    }

    // Parse 'nix' (required)
    let nix = m
        .get("nix")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("spec must have a 'nix' directive (flake ref)"))?
        .to_string();

    // Parse 'files' (optional)
    let files = if let Some(files_val) = m.get("files") {
        let seq = files_val
            .as_sequence()
            .ok_or_else(|| anyhow::anyhow!("'files' must be a list"))?;
        let mut files = Vec::new();
        for entry in seq {
            let path = entry
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| anyhow::anyhow!("each file entry must have 'path'"))?
                .to_string();
            let content = entry
                .get("content")
                .and_then(|c| c.as_str())
                .ok_or_else(|| anyhow::anyhow!("each file entry must have 'content'"))?
                .to_string();
            // Security: reject absolute paths and path traversal (spec §8 Fixture path escape)
            if path.starts_with('/') {
                return Err(anyhow::anyhow!(
                    "file path '{}' must be relative (no absolute paths allowed)", path
                ));
            }
            if path.split('/').any(|c| c == "..") {
                return Err(anyhow::anyhow!(
                    "file path '{}' contains '..' traversal components (not allowed)", path
                ));
            }
            files.push(FilesEntry { path, content });
        }
        files
    } else {
        Vec::new()
    };

    // Parse 'env' (optional)
    let env = if let Some(env_val) = m.get("env") {
        let env_m = env_val
            .as_mapping()
            .ok_or_else(|| anyhow::anyhow!("'env' must be a mapping"))?;
        let mut env = HashMap::new();
        for (k, v) in env_m {
            let key = k
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("env key must be a string"))?
                .to_string();
            let val = v
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("env value for '{key}' must be a string"))?
                .to_string();
            env.insert(key, val);
        }
        env
    } else {
        HashMap::new()
    };

    // Parse 'nodes' (optional — a spec can have no nodes at the lint stage)
    let nodes = if let Some(nodes_val) = m.get("nodes") {
        let seq = nodes_val
            .as_sequence()
            .ok_or_else(|| anyhow::anyhow!("'nodes' must be a list"))?;
        let mut nodes = Vec::new();
        for node_val in seq {
            let node = parse_node(node_val)?;
            nodes.push(node);
        }
        nodes
    } else {
        Vec::new()
    };

    // Parse top-level 'adapters' (optional — applies to all nodes unless overridden per-node)
    let adapters = if let Some(ad_val) = m.get("adapters") {
        Some(parse_adapters(ad_val)?)
    } else {
        None
    };

    Ok(SpecDoc { nix, files, env, nodes, adapters })
}

fn parse_node(v: &serde_yaml::Value) -> Result<NodeSpec> {
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| anyhow::anyhow!("node must have 'name'"))?
        .to_string();

    let argv_val = v
        .get("argv")
        .ok_or_else(|| anyhow::anyhow!("node '{name}' must have 'argv'"))?;
    let argv = argv_val
        .as_sequence()
        .ok_or_else(|| anyhow::anyhow!("node '{name}'.argv must be a list"))?
        .iter()
        .map(|a| {
            a.as_str()
                .ok_or_else(|| anyhow::anyhow!("argv entries must be strings"))
                .map(|s| s.to_string())
        })
        .collect::<Result<Vec<_>>>()?;

    let adapters = if let Some(ad_val) = v.get("adapters") {
        parse_adapters(ad_val)?
    } else {
        crate::adapter::Adapter::default()
    };

    // Check for unknown keys in node
    if let Some(m) = v.as_mapping() {
        for (k, _) in m {
            let key = k.as_str().unwrap_or_default();
            if !["name", "argv", "adapters"].contains(&key) {
                bail!("unknown key '{key}' in node '{name}'; valid: name, argv, adapters");
            }
        }
    }

    Ok(NodeSpec { name, argv, adapters })
}
