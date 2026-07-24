// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud verify — determinism and observation verification commands (M3)

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct VerifyCmd {
    #[command(subcommand)]
    pub action: VerifyAction,
}

#[derive(Subcommand)]
pub enum VerifyAction {
    /// Verify that a spec is deterministic: run it twice with the same seed and compare observation stream hashes.
    /// Exit code 0 = deterministic, 1 = non-deterministic or error.
    Determinism {
        /// Path to spec YAML file
        #[arg(long)]
        spec: String,
        /// RNG seed (default 0)
        #[arg(long)]
        seed: Option<u64>,
        /// Number of runs to compare (minimum 2, default 2)
        #[arg(long, default_value = "2")]
        times: u32,
        /// Test the poisoned variant (injects time-based nondeterminism, should fail)
        #[arg(long, hide = true)]
        poisoned: bool,
    },
    /// Cross-check syscall log vs eBPF observation plane for a run (M7+).
    Observation {
        /// Run ID to verify
        #[arg(long)]
        run: String,
    },
}

pub async fn run(cmd: VerifyCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        VerifyAction::Determinism { spec, seed, times, poisoned } => {
            // Read spec file
            let spec_content = std::fs::read_to_string(&spec)
                .map_err(|e| anyhow::anyhow!("failed to read spec file {spec}: {e}"))?;

            let endpoint = if poisoned {
                "/verify/determinism/poisoned"
            } else {
                "/verify/determinism"
            };

            let body = json!({
                "spec": spec_content,
                "seed": seed.unwrap_or(0),
                "times": times,
            });

            let v = c.post(endpoint, &body).await?;

            let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
            fmt::print(&v, json);

            if !ok {
                std::process::exit(1);
            }
        }
        VerifyAction::Observation { run } => {
            let v = c.get(&format!("/verify/observation/{run}")).await?;
            fmt::print(&v, json);
        }
    }
    Ok(())
}
