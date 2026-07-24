// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud tape — tape (sandbox) lifecycle subcommand

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use crate::client::Client;
use crate::fmt::print_value;

#[derive(Parser)]
pub struct TapeCmd {
    #[command(subcommand)]
    pub action: TapeAction,
}

#[derive(Subcommand)]
pub enum TapeAction {
    /// Create a new tape (sandbox)
    Create {
        /// Backend: "local" (default) or "daytona"
        #[arg(long, default_value = "local")]
        backend: String,
        /// Optional image/snapshot ID
        #[arg(long)]
        image: Option<String>,
    },
    /// List all tapes
    Ls,
    /// Get tape status
    Status {
        /// Tape ID
        id: String,
    },
    /// Ensure tape is running (start if stopped, restore if archived)
    Ensure {
        /// Tape ID
        id: String,
    },
    /// Kill (permanently delete) a tape
    Kill {
        /// Tape ID
        id: String,
    },
    /// Reconstruct a tape from journal (stub — M6)
    Reconstruct {
        /// Tape ID
        id: String,
    },
    /// Execute a command inside the tape
    Exec {
        /// Tape ID
        id: String,
        /// Command and arguments
        cmd: Vec<String>,
    },
    /// Query probe capabilities of the tape
    ProbeCaps {
        /// Tape ID
        id: String,
    },
}

pub async fn run(cmd: TapeCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        TapeAction::Create { backend, image } => {
            let mut body = serde_json::json!({ "backend": backend });
            if let Some(img) = image {
                body["image"] = json!(img);
            }
            let v = c.post("/tapes", &body).await?;
            print_value(&v, json);
        }
        TapeAction::Ls => {
            let v = c.get("/tapes").await?;
            if json {
                print_value(&v, true);
            } else {
                // Pretty-print table
                if let Some(tapes) = v.get("tapes").and_then(|t| t.as_array()) {
                    if tapes.is_empty() {
                        println!("No tapes.");
                    } else {
                        println!("{:<30}  {:<8}  {:<10}  {}", "ID", "BACKEND", "STATE", "CREATED");
                        for t in tapes {
                            let id = t["id"].as_str().unwrap_or("-");
                            let backend = t["backend"].as_str().unwrap_or("-");
                            let state = t["state"].as_str().unwrap_or("-");
                            let ca = t["created_at"].as_i64().unwrap_or(0);
                            println!("{:<30}  {:<8}  {:<10}  {}", id, backend, state, ca);
                        }
                    }
                } else {
                    print_value(&v, false);
                }
            }
        }
        TapeAction::Status { id } => {
            let v = c.get(&format!("/tapes/{id}")).await?;
            print_value(&v, json);
        }
        TapeAction::Ensure { id } => {
            let v = c.post(&format!("/tapes/{id}/ensure"), &json!({})).await?;
            print_value(&v, json);
        }
        TapeAction::Kill { id } => {
            let v = c.delete(&format!("/tapes/{id}")).await?;
            print_value(&v, json);
        }
        TapeAction::Reconstruct { id } => {
            eprintln!("tape reconstruct {id}: not yet implemented (M6)");
        }
        TapeAction::Exec { id, cmd } => {
            if cmd.is_empty() {
                anyhow::bail!("exec: no command specified");
            }
            let v = c.post(&format!("/tapes/{id}/exec"), &json!({ "cmd": cmd })).await?;
            if json {
                print_value(&v, true);
            } else {
                // Print stdout/stderr like a shell
                if let Some(stdout) = v["stdout"].as_str() {
                    if !stdout.is_empty() {
                        print!("{}", stdout);
                    }
                }
                if let Some(stderr) = v["stderr"].as_str() {
                    if !stderr.is_empty() {
                        eprint!("{}", stderr);
                    }
                }
                if let Some(code) = v["exit_code"].as_i64() {
                    if code != 0 {
                        anyhow::bail!("exit code {code}");
                    }
                }
            }
        }
        TapeAction::ProbeCaps { id } => {
            let v = c.get(&format!("/tapes/{id}/endpoint")).await?;
            print_value(&v, json);
        }
    }
    Ok(())
}
