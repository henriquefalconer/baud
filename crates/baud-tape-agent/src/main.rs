// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-tape-agent — the in-sandbox agent
//
// Runs inside a Daytona sandbox (or local VM). Lifecycle:
//   1. Read workload spec from stdin (CBOR-encoded Hello{identity, manifest_hash}).
//   2. Run baud-init provisioning (nix build, file fixtures, env, adapters).
//   3. Launch baud-multiverse with the spec's node topology.
//   4. Relay DrawRequest / DrawResult messages between the supervisor and baud-server.
//   5. Apply input adapters (stdin, fifo, net) to guest processes.
//   6. Sample probe adapters (stdout-kv, exit-hash, ...) and emit Observe records.
//   7. Stream observations + syscall records outbound (WebSocket or exec/file fallback).
//   8. On Eof or SIGTERM: flush journal, terminate supervisor, exit.
//
// This crate contains no workload logic — everything is declared in the spec's adapters.
// A new workload kind requires zero agent changes (tested by M8: Mario spec runs on
// the agent binary built at M2, unmodified).

mod agent;
mod protocol;
mod transport;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize structured logging (stderr)
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    agent::run().await
}
