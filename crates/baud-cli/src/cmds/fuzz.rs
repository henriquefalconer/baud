// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud fuzz — fuzz loop commands (M4)

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct FuzzCmd {
    #[command(subcommand)]
    pub action: FuzzAction,
}

#[derive(Subcommand)]
pub enum FuzzAction {
    /// Start a fuzz session on the parser workload
    Start {
        /// Path to spec file (YAML)
        #[arg(long)]
        spec: String,
        /// Tactics: "random" or "stateful-mask"
        #[arg(long, default_value = "random")]
        tactics: String,
        /// Strategy JSON (inline or @path)
        #[arg(long)]
        strategy: Option<String>,
        /// RNG seed
        #[arg(long, default_value = "0")]
        seed: u64,
        /// Maximum fuzz iterations
        #[arg(long, default_value = "200")]
        max_iterations: u32,
        /// Stop when crash (goal) is found
        #[arg(long, default_value = "true")]
        stop_on_crash: bool,
    },
    /// Get fuzz session status
    Status {
        /// Fuzz session / run ID
        id: String,
    },
}

pub async fn run(cmd: FuzzCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        FuzzAction::Start {
            spec,
            tactics,
            strategy,
            seed,
            max_iterations,
            stop_on_crash,
        } => {
            let spec_content = std::fs::read_to_string(&spec)
                .map_err(|e| anyhow::anyhow!("failed to read spec '{}': {}", spec, e))?;

            let body = json!({
                "spec": spec_content,
                "tactics": tactics,
                "strategy": strategy,
                "seed": seed,
                "max_iterations": max_iterations,
                "stop_on_crash": stop_on_crash,
            });

            let v = c.post("/runs/fuzz", &body).await?;
            fmt::print(&v, json);

            // Exit with code 2 if goal reached
            if v.get("goal_reached").and_then(|x| x.as_bool()).unwrap_or(false) {
                std::process::exit(2);
            }
            if v.get("error").is_some() {
                std::process::exit(1);
            }
        }
        FuzzAction::Status { id } => {
            let v = c.get(&format!("/runs/fuzz/{id}")).await?;
            fmt::print(&v, json);
        }
    }
    Ok(())
}
