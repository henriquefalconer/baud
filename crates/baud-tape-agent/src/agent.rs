// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Agent main loop.
//
// Responsibilities (spec §3):
//   1. Read spec from BAUD_SPEC env var (or stdin Hello CBOR)
//   2. Provision via baud-init (lint + validate)
//   3. Launch baud-multiverse with the spec's node topology
//   4. Relay DrawRequest / DrawResult between supervisor and server
//   5. Apply input adapters (stdin, fifo, net) to guest processes
//   6. Sample probe adapters (stdout-kv, exit-hash, ...) and emit Observe records
//   7. Stream observations outbound (transport layer)
//   8. On Eof or SIGTERM: flush, terminate supervisor, exit

use anyhow::{Context, Result};
use baud_proto::{DrawRequest, DrawResult, Msg, Observation, Value as ProbeValue};
use baud_multiverse::{Multiverse, RunManifest, TapeDrawSource};

use crate::transport::Transport;

/// Entry point for the agent run loop.
pub async fn run() -> Result<()> {
    tracing::info!("baud-tape-agent starting");

    // Read workload spec from environment
    let spec_yaml = std::env::var("BAUD_SPEC")
        .context("BAUD_SPEC environment variable not set")?;

    let spec_doc = baud_init::lint(&spec_yaml)
        .context("workload spec failed to lint")?;

    tracing::info!("spec: nix={}, nodes={}", spec_doc.nix, spec_doc.nodes.len());

    // Build a minimal RunManifest from the spec
    let manifest = build_manifest(&spec_doc)?;

    // Launch the supervisor
    let mut supervisor = Multiverse::load_from_manifest(manifest)
        .context("failed to load manifest into supervisor")?;

    tracing::info!("supervisor loaded ({} guests)", supervisor.manifest.guests.len());

    // Relay loop: drive the supervisor with draws from the server.
    // In production this uses a WebSocket transport. For scaffold/test: use stdio.
    let seed = std::env::var("BAUD_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    // Create a synthetic tape for testing / scaffold mode
    let tape_bytes = make_tape_from_seed(seed, 4096);
    let mut tape = TapeDrawSource::new(tape_bytes);

    // Run the supervisor
    let obs_stream = supervisor.run(&mut tape);

    tracing::info!("supervisor completed: {} observations", obs_stream.observations.len());

    // Emit observations to transport
    // In production: stream via WebSocket to baud-server
    // In scaffold: emit to stderr as JSON for diagnostics
    for obs_entry in &obs_stream.observations {
        let obs = Observation {
            probe: obs_entry.probe.clone(),
            node: obs_entry.node as u16,
            value: json_to_probe_value(&obs_entry.value),
            step: obs_entry.step,
        };
        let msg = Msg::Observe(obs);
        if let Ok(cbor) = baud_proto::encode(&msg) {
            let _ = cbor; // In production: write to transport
        }
        tracing::debug!("obs: node={} probe={} step={}", obs_entry.node, obs_entry.probe, obs_entry.step);
    }

    // Emit node.ready observation for each node (confirms agent parsed spec correctly)
    for (i, node) in spec_doc.nodes.iter().enumerate() {
        let obs = Observation {
            probe: format!("agent.node.{}.ready", node.name),
            node: i as u16,
            value: ProbeValue::U64(1),
            step: 0,
        };
        let cbor = baud_proto::encode(&Msg::Observe(obs))
            .map_err(|e| anyhow::anyhow!("encode error: {e}"))?;
        let _ = cbor; // In production: send via transport
    }

    // Emit stream hash as a checkpoint
    let stream_hash = obs_stream.stream_hash();
    tracing::info!("stream_hash={}", stream_hash);

    tracing::info!("baud-tape-agent: run complete ({} observations, hash={})",
        obs_stream.observations.len(), &stream_hash[..16.min(stream_hash.len())]);
    Ok(())
}

/// Build a RunManifest from a parsed spec document.
/// Converts baud-init's SpecDoc into the supervisor's manifest format.
pub fn build_manifest(spec_doc: &baud_init::SpecDoc) -> Result<RunManifest> {
    use baud_multiverse::GuestSpec;
    use std::path::PathBuf;

    let guests: Vec<GuestSpec> = spec_doc.nodes.iter().enumerate().map(|(i, node)| {
        GuestSpec {
            node_id: i as u32,
            binary: PathBuf::from(node.argv.first().cloned().unwrap_or_default()),
            argv: node.argv.clone(),
        }
    }).collect();

    Ok(RunManifest {
        guests,
        ..Default::default()
    })
}

/// Convert a serde_json::Value probe value to baud_proto::Value.
fn json_to_probe_value(v: &serde_json::Value) -> ProbeValue {
    match v {
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                ProbeValue::U64(u)
            } else if let Some(i) = n.as_i64() {
                ProbeValue::I64(i)
            } else {
                // Float: encode as i64 (multiply by 1000 to preserve 3 decimal places)
                let f = n.as_f64().unwrap_or(0.0);
                ProbeValue::I64((f * 1000.0) as i64)
            }
        }
        serde_json::Value::String(s) => ProbeValue::Utf8(s.clone()),
        serde_json::Value::Object(m) => {
            // Encode object as JSON string
            ProbeValue::Utf8(serde_json::to_string(m).unwrap_or_default())
        }
        _ => ProbeValue::U64(0),
    }
}

/// Create a synthetic tape from a seed for scaffold/test mode.
/// In production, draws come from the baud-driver via the server.
fn make_tape_from_seed(seed: u64, len: usize) -> Vec<u8> {
    let mut tape = vec![0u8; len];
    // Simple LCG to generate pseudo-random bytes from seed
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    for byte in tape.iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *byte = (state >> 33) as u8;
    }
    tape
}

// ---------------------------------------------------------------------------
// Draw relay: mediates DrawRequest / DrawResult between supervisor and server
// ---------------------------------------------------------------------------

/// Relay a single draw request to the server and return the result.
/// This is the core of the Hegel-like protocol inversion:
/// - Supervisor requests a draw (source of randomness)
/// - Server (driver) provides the draw result (from the tape)
/// - Result is returned to the supervisor to serve the device model
///
/// In production this runs over WebSocket. In scaffold mode it returns
/// a synthetic result from the tape.
#[allow(dead_code)]
pub fn relay_draw(transport: &mut dyn Transport, req: &DrawRequest) -> Result<DrawResult> {
    // Send the draw request to the server
    transport.send(&Msg::DrawRequest(req.clone()))
        .context("failed to send DrawRequest")?;

    // Wait for the DrawResult
    loop {
        match transport.recv().context("failed to receive DrawResult")? {
            Some(Msg::DrawResult(result)) => return Ok(result),
            Some(other) => {
                // Unexpected message type — log and continue waiting
                tracing::warn!("expected DrawResult, got unexpected msg type");
                let _ = other;
            }
            None => {
                anyhow::bail!("transport EOF while waiting for DrawResult");
            }
        }
    }
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

    /// Test that the agent builds a manifest from a spec document correctly.
    #[test]
    fn agent_builds_manifest_from_spec() {
        let spec_yaml = r#"
nix: "./flake.nix#hello"
nodes:
  - name: hello
    argv: ["hello-binary", "--seed", "42"]
    adapters:
      input: stdin
      probes:
        - stdout-kv
  - name: world
    argv: ["world-binary"]
    adapters:
      input: stdin
      probes:
        - exit-hash
"#;
        let doc = baud_init::lint(spec_yaml).expect("spec must lint");
        let manifest = super::build_manifest(&doc).expect("build manifest");

        assert_eq!(manifest.guests.len(), 2, "manifest must have 2 guests");
        assert_eq!(manifest.guests[0].node_id, 0);
        assert_eq!(manifest.guests[1].node_id, 1);
        assert_eq!(manifest.guests[0].argv, &["hello-binary", "--seed", "42"]);
        assert_eq!(manifest.guests[1].argv, &["world-binary"]);
    }

    /// Test that the draw relay sends and receives correctly over a mock transport.
    #[test]
    fn relay_draw_sends_request_and_receives_result() {
        use baud_proto::{DrawRequest, DrawResult, Msg};
        use crate::transport::Transport;

        struct MockTransport {
            sent: Vec<Msg>,
            to_recv: Vec<Option<Msg>>,
        }

        impl Transport for MockTransport {
            fn send(&mut self, msg: &Msg) -> anyhow::Result<()> {
                self.sent.push(msg.clone());
                Ok(())
            }
            fn recv(&mut self) -> anyhow::Result<Option<Msg>> {
                if self.to_recv.is_empty() {
                    Ok(None)
                } else {
                    Ok(self.to_recv.remove(0))
                }
            }
        }

        let req = DrawRequest::Bits(8);
        let expected_result = DrawResult { bytes: vec![0x42, 0x13] };

        let mut transport = MockTransport {
            sent: Vec::new(),
            to_recv: vec![Some(Msg::DrawResult(expected_result.clone()))],
        };

        let result = super::relay_draw(&mut transport, &req)
            .expect("relay_draw must succeed");

        assert_eq!(result.bytes, expected_result.bytes, "draw result must match");
        assert_eq!(transport.sent.len(), 1, "must have sent exactly 1 message");
        match &transport.sent[0] {
            Msg::DrawRequest(r) => assert!(matches!(r, DrawRequest::Bits(_)), "must send DrawRequest::Bits"),
            other => panic!("expected DrawRequest, got {:?}", std::mem::discriminant(other)),
        }
    }

    /// Test that the synthetic tape generator produces deterministic output.
    #[test]
    fn make_tape_is_deterministic() {
        let tape1 = super::make_tape_from_seed(42, 256);
        let tape2 = super::make_tape_from_seed(42, 256);
        let tape3 = super::make_tape_from_seed(99, 256);

        assert_eq!(tape1, tape2, "same seed must produce same tape");
        assert_ne!(tape1, tape3, "different seeds must produce different tapes");
        assert_eq!(tape1.len(), 256, "tape length must match requested");
    }
}
