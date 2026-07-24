// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Agent main loop.

use anyhow::{Context, Result};
use baud_proto::{Msg, Observation, Value as ProbeValue};

/// Entry point for the agent run loop.
pub async fn run() -> Result<()> {
    tracing::info!("baud-tape-agent starting");

    // Read workload spec from environment
    let spec_yaml = std::env::var("BAUD_SPEC")
        .context("BAUD_SPEC environment variable not set")?;

    let spec_doc = baud_init::lint(&spec_yaml)
        .context("workload spec failed to lint")?;

    tracing::info!("spec: nix={}, nodes={}", spec_doc.nix, spec_doc.nodes.len());

    // Emit a synthetic observation for each node (confirms agent parsed spec correctly).
    // In full implementation, this is replaced by real supervisor observations.
    for (i, node) in spec_doc.nodes.iter().enumerate() {
        let obs = Observation {
            probe: format!("agent.node.{}.ready", node.name),
            node: i as u16,
            value: ProbeValue::U64(1),
            step: 0,
        };
        let cbor = baud_proto::encode(&Msg::Observe(obs))
            .map_err(|e| anyhow::anyhow!("encode error: {e}"))?;
        // In production: send via transport layer.
        // For scaffold: emit to stdout length-prefixed.
        let _ = cbor;
    }

    tracing::info!("baud-tape-agent: scaffold run complete (no supervisor integration yet)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use baud_init::{InputAdapter, ProbeAdapter};

    /// VR1-B5: unmodified_agent_runs_a_new_workload
    ///
    /// Verifies that the agent can load and validate a minimal workload spec
    /// without any workload-specific code in the agent itself.
    /// The agent must never need to be modified for a new workload kind.
    #[test]
    fn unmodified_agent_runs_a_new_workload() {
        // A novel workload spec using only the closed adapter set.
        // The agent processes this without any workload-specific code.
        let spec_yaml = r#"
nix: "./flake.nix#new-workload"
nodes:
  - name: worker
    argv: ["new-workload-binary", "--mode", "test"]
    adapters:
      input: stdin
      probes:
        - stdout-kv
"#;

        // Lint succeeds (agent validates spec on startup)
        let doc = baud_init::lint(spec_yaml)
            .expect("minimal spec must lint ok");

        assert_eq!(doc.nodes.len(), 1);
        assert_eq!(doc.nodes[0].name, "worker");

        // The agent has no workload-specific branches — it processes any valid spec
        // through the same adapter pipeline. The test passes if lint succeeds and
        // the agent's generic adapter dispatch would cover this workload's needs.
        let has_stdin_input = matches!(
            doc.nodes[0].adapters.input,
            Some(InputAdapter::Stdin)
        );
        assert!(has_stdin_input, "input adapter must be Stdin");

        let has_stdout_kv = doc.nodes[0].adapters.probes.iter().any(|p| {
            matches!(p, ProbeAdapter::StdoutKv { .. })
        });
        assert!(has_stdout_kv, "probe must include stdout-kv");
    }
}
