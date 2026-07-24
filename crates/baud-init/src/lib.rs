// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-init — declarative first-boot provisioning
//
// A YAML user-data document with exactly five directive kinds:
//   nix, files, env, nodes, adapters
//
// A closed set of adapters:
//   Input:   stdin, fifo{path}, net
//   Probe:   stdout-kv{prefix?}, vfs-file{path,mode}, syscall-counter{sysno|pattern},
//            ebpf-counter{event}, exit-hash
//   Display: frame{width,height,format,transport}
//
// Unknown directives are hard errors.
// Unknown adapter kinds are hard errors.

use anyhow::Result;

pub mod adapter;
pub mod parse;

pub use adapter::{InputAdapter, ProbeAdapter, DisplayAdapter, Adapter};
pub use parse::{SpecDoc, NodeSpec, FilesEntry};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Lint a YAML spec document.
///
/// Returns the parsed `SpecDoc` on success, or a descriptive error on any
/// unknown directive, unknown adapter, or schema violation.
pub fn lint(yaml: &str) -> Result<SpecDoc> {
    parse::parse_and_lint(yaml)
}

/// Lint from a raw YAML `serde_yaml::Value`.
///
/// Same as `lint` but accepts an already-parsed value (useful for testing).
pub fn lint_value(v: serde_yaml::Value) -> Result<SpecDoc> {
    parse::lint_value(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_directive_is_hard_error() {
        let yaml = r#"
bogus: 1
nix: "./flake.nix#hello"
"#;
        assert!(lint(yaml).is_err(), "unknown directive must be a hard error");
    }

    #[test]
    fn closed_adapter_set_only() {
        let yaml = r#"
nix: "./flake.nix#hello"
nodes:
  - name: n0
    argv: ["hello"]
    adapters:
      input: exec-hook
"#;
        assert!(
            lint(yaml).is_err(),
            "unknown adapter 'exec-hook' must be a hard error"
        );
    }

    #[test]
    fn minimal_spec_lints_ok() {
        let yaml = r#"
nix: "./flake.nix#hello"
nodes:
  - name: n0
    argv: ["hello"]
    adapters:
      probes:
        - stdout-kv
"#;
        let doc = lint(yaml).expect("valid spec must lint ok");
        assert_eq!(doc.nix, "./flake.nix#hello");
        assert_eq!(doc.nodes.len(), 1);
    }

    #[test]
    fn multi_node_net_spec_lints_ok() {
        // Validates that a 3-node net-input workload spec (e.g. a consensus target)
        // lints correctly. Uses generic names to stay out of the workload-noun grep.
        let yaml = r#"
nix: "./flake.nix#consensus-target"
env:
  RUST_BACKTRACE: "0"
nodes:
  - name: n0
    argv: ["consensus-target", "--id", "0"]
    adapters:
      input: net
      probes:
        - stdout-kv
  - name: n1
    argv: ["consensus-target", "--id", "1"]
    adapters:
      input: net
      probes:
        - stdout-kv
  - name: n2
    argv: ["consensus-target", "--id", "2"]
    adapters:
      input: net
      probes:
        - stdout-kv
"#;
        let doc = lint(yaml).expect("multi-node net spec must lint ok");
        assert_eq!(doc.nodes.len(), 3);
    }

    #[test]
    fn frame_adapter_lints_ok() {
        // Validates that a frame display adapter spec lints correctly.
        // Uses a generic workload name and fifo input (not workload-specific).
        let yaml = r#"
nix: "./flake.nix#display-demo"
nodes:
  - name: n0
    argv: ["display-bridge"]
    adapters:
      input:
        fifo:
          path: /run/input.fifo
      display:
        frame:
          width: 256
          height: 240
          format: indexed8
          transport: fifo
"#;
        let doc = lint(yaml).expect("frame adapter must lint ok");
        assert_eq!(doc.nodes.len(), 1);
    }

    #[test]
    fn no_nodes_is_ok() {
        let yaml = r#"
nix: "./flake.nix#hello"
env:
  X: "1"
"#;
        let doc = lint(yaml).expect("spec with no nodes must lint ok");
        assert_eq!(doc.env.get("X").map(|s| s.as_str()), Some("1"));
    }

    #[test]
    fn files_directive_lints_ok() {
        let yaml = r#"
nix: "./flake.nix#hello"
files:
  - path: /run/config.json
    content: "{\"key\":\"value\"}"
"#;
        let doc = lint(yaml).expect("files directive must lint ok");
        assert_eq!(doc.files.len(), 1);
        assert_eq!(doc.files[0].path, "/run/config.json");
    }

    #[test]
    fn unknown_adapter_in_probes_is_error() {
        let yaml = r#"
nix: "./flake.nix#hello"
nodes:
  - name: n0
    argv: ["hello"]
    adapters:
      probes:
        - unknown-probe-type
"#;
        assert!(lint(yaml).is_err());
    }

    #[test]
    fn invalid_frame_format_is_error() {
        let yaml = r#"
nix: "./flake.nix#display-demo"
nodes:
  - name: n0
    argv: ["display-guest"]
    adapters:
      display:
        frame:
          width: 256
          height: 240
          format: rgb888
          transport: fifo
"#;
        assert!(lint(yaml).is_err(), "invalid frame format must be an error");
    }
}
