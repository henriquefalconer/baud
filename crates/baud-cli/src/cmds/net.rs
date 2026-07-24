// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud net — network weather timeline commands (M5)

use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct NetCmd {
    #[command(subcommand)]
    pub action: NetAction,
}

#[derive(Subcommand)]
pub enum NetAction {
    /// Print the recorded partition/delay weather timeline for a run
    Weather {
        #[arg(long)]
        run: String,
    },
}

pub async fn run(cmd: NetCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        NetAction::Weather { run } => {
            let resp: serde_json::Value = c.get(&format!("/runs/{run}/net/weather")).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                let events = resp["weather"].as_array().cloned().unwrap_or_default();
                if events.is_empty() {
                    println!("No net weather events recorded for run {run}");
                } else {
                    println!("Net weather timeline — run={run}");
                    println!("{:<8} {:<18} {}", "step", "kind", "detail");
                    println!("{}", "-".repeat(50));
                    for ev in &events {
                        let step = ev["step"].as_i64().unwrap_or(0);
                        let kind = ev["kind"].as_str().unwrap_or("?");
                        let detail = match kind {
                            "delay" => {
                                let from = ev["from_node"].as_i64().unwrap_or(-1);
                                let to = ev["to_node"].as_i64().unwrap_or(-1);
                                let ticks = ev["delay_ticks"].as_i64().unwrap_or(0);
                                format!("node{from}→node{to} delay={ticks} ticks")
                            }
                            "drop" => {
                                let prob = ev["drop_prob"].as_f64().unwrap_or(0.0);
                                format!("drop_prob={prob:.3}")
                            }
                            _ => String::new(),
                        };
                        println!("{:<8} {:<18} {}", step, kind, detail);
                    }
                    println!("  {} event(s) total", events.len());
                }
            }
        }
    }
    Ok(())
}
