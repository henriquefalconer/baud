// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud run — run management commands

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct RunCmd {
    #[command(subcommand)]
    pub action: RunAction,
}

#[derive(Subcommand)]
pub enum RunAction {
    /// Start a new run
    Start {
        /// Path to spec file (YAML)
        #[arg(long)]
        spec: String,
        /// Strategy spec (inline JSON or @path)
        #[arg(long)]
        strategy: Option<String>,
        /// Tactics spec (inline JSON or @path)
        #[arg(long)]
        tactics: Option<String>,
        /// RNG seed
        #[arg(long)]
        seed: Option<u64>,
        /// Budget in minutes
        #[arg(long)]
        budget_minutes: Option<u64>,
        /// Backend: "local" or "daytona"
        #[arg(long, default_value = "local")]
        backend: String,
    },
    /// List runs
    Ls,
    /// Show run status
    Status {
        /// Run ID
        run: String,
    },
    /// Watch a run (streaming output)
    Watch {
        /// Run ID
        run: String,
    },
    /// Pause a run
    Pause {
        /// Run ID
        run: String,
    },
    /// Resume a paused run
    Resume {
        /// Run ID
        run: String,
    },
    /// Abort a run
    Abort {
        /// Run ID
        run: String,
    },
    /// Boot a guest image directly on the real KVM Multiverse and run it to its first halt
    /// (H0-H6's post-pivot core — bypasses the sandbox/spec/tape machinery above entirely).
    Kvm {
        /// Path to a bzImage kernel on the server host's filesystem.
        #[arg(long)]
        kernel: String,
        /// Kernel command line.
        #[arg(long, default_value = "console=ttyS0")]
        cmdline: String,
        /// The run's whole tape, hex-encoded.
        #[arg(long, default_value = "")]
        tape_hex: String,
    },
}

pub async fn run(cmd: RunCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        RunAction::Start {
            spec,
            strategy,
            tactics,
            seed,
            budget_minutes,
            backend,
        } => {
            let spec_content = std::fs::read_to_string(&spec)
                .map_err(|e| anyhow::anyhow!("failed to read spec '{}': {}", spec, e))?;
            let body = json!({
                "spec": spec_content,
                "strategy": strategy,
                "tactics": tactics,
                "seed": seed.unwrap_or(0),
                "budget_minutes": budget_minutes.unwrap_or(60),
                "backend": backend,
            });
            let v = c.post("/runs", &body).await?;
            fmt::print(&v, json);
            if v.get("error").is_some() {
                std::process::exit(1);
            }
        }
        RunAction::Ls => {
            let v = c.get("/runs").await?;
            fmt::print(&v, json);
        }
        RunAction::Status { run: id } => {
            let v = c.get(&format!("/runs/{id}")).await?;
            fmt::print(&v, json);
            if v.get("error").is_some() {
                std::process::exit(1);
            }
            // Exit code 2 when the run found a bug / goal (baud-cli.md §4 exit codes)
            let status_str = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if matches!(status_str, "crashed" | "goal" | "violation_found") {
                std::process::exit(2);
            }
        }
        RunAction::Abort { run: id } => {
            let v = c.post(&format!("/runs/{id}/abort"), &json!({})).await?;
            fmt::print(&v, json);
        }
        RunAction::Watch { run: id } => {
            // Stub: poll status (SSE in M3+)
            let v = c.get(&format!("/runs/{id}")).await?;
            fmt::print(&v, json);
        }
        RunAction::Pause { run: id } => {
            eprintln!("run pause {id}: not yet implemented (M4+)");
        }
        RunAction::Resume { run: id } => {
            eprintln!("run resume {id}: not yet implemented (M4+)");
        }
        RunAction::Kvm { kernel, cmdline, tape_hex } => {
            let body = json!({
                "kernel_path": kernel,
                "cmdline": cmdline,
                "tape_hex": tape_hex,
            });
            let v = c.post("/run/kvm", &body).await?;
            fmt::print(&v, json);
            if v.get("error").is_some() {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
