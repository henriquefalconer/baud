// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Flake template generation for baud-packages.
//
// One flake template + substitution; the pinned nixpkgs rev lives in one place.
// No Nix-language AST manipulation.

use anyhow::Result;
use crate::spec::WorkloadSpec;

pub struct FlakeTemplate;

impl FlakeTemplate {
    /// Generate a `flake.nix` string for the given workload spec.
    ///
    /// The generated flake:
    ///   - Pins nixpkgs to the given rev
    ///   - Produces a static, no-PIE, musl-built guest binary
    ///   - Exposes the binary as the `guest` output
    pub fn generate(spec: &WorkloadSpec, nixpkgs_rev: &str) -> Result<String> {
        let name = &spec.workload.name;
        let packages = spec.workload.packages.join(" ");
        let build_cmd = &spec.workload.build;

        // Simple template substitution — no AST manipulation
        let flake = format!(
            r#"{{
  description = "baud workload: {name}";

  inputs = {{
    nixpkgs.url = "github:NixOS/nixpkgs/{nixpkgs_rev}";
    flake-utils.url = "github:numtide/flake-utils";
  }};

  outputs = {{ self, nixpkgs, flake-utils }}:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {{ inherit system; }};
        musl-pkgs = pkgs.pkgsMusl;
      in {{
        packages.guest = musl-pkgs.stdenv.mkDerivation {{
          name = "{name}";
          src = ./.;
          nativeBuildInputs = with musl-pkgs; [ {packages} ];
          buildPhase = ''
            {build_cmd}
          '';
          installPhase = ''
            mkdir -p $out/bin
            cp guest $out/bin/{name}
          '';
          # Enforce no-PIE for determinism
          hardeningDisable = [ "pie" ];
        }};
      }}
    );
}}
"#
        );

        Ok(flake)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{WorkloadSpec, WorkloadPackage};

    fn make_spec(name: &str) -> WorkloadSpec {
        WorkloadSpec {
            workload: WorkloadPackage {
                name: name.to_string(),
                packages: vec!["stdenv".to_string(), "musl".to_string()],
                build: "cc -static -no-pie -o guest main.c".to_string(),
            },
        }
    }

    #[test]
    fn flake_contains_name() {
        let spec = make_spec("parser");
        let flake = FlakeTemplate::generate(&spec, "23.11").unwrap();
        assert!(flake.contains("parser"));
        assert!(flake.contains("23.11"));
        assert!(flake.contains("cc -static -no-pie"));
    }

    #[test]
    fn flake_no_pie_hardening() {
        let spec = make_spec("hello");
        let flake = FlakeTemplate::generate(&spec, "23.11").unwrap();
        assert!(flake.contains(r#"hardeningDisable = [ "pie" ]"#));
    }
}
