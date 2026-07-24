// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud tracing — observation plane 2 (M7)
//
// baud tracing tail --tape <id> [--event sched|syscall|exec|fault] [--node <n>]
// baud tracing summary --run <id>

use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct TracingCmd {
    #[command(subcommand)]
    pub action: TracingAction,
}

#[derive(Subcommand)]
pub enum TracingAction {
    /// Stream live eBPF events from a tape
    Tail {
        #[arg(long)]
        tape: String,
        #[arg(long, help = "Filter by event kind: sched|syscall|exec|fault")]
        event: Option<String>,
        #[arg(long, help = "Filter by node index")]
        node: Option<u16>,
    },
    /// Summarize tracing data for a run
    Summary {
        #[arg(long)]
        run: String,
    },
}

pub async fn run(cmd: TracingCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        TracingAction::Tail { tape, event, node } => {
            let mut url = format!("/tracing/tail?tape={tape}");
            if let Some(ev) = &event { url.push_str(&format!("&event={ev}")); }
            if let Some(n) = node { url.push_str(&format!("&node={n}")); }
            let resp = c.get(&url).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                let count = resp["count"].as_u64().unwrap_or(0);
                let source = resp["records"].as_array()
                    .and_then(|a| a.first())
                    .and_then(|r| r["source"].as_str())
                    .unwrap_or("fallback");
                println!("tracing tail: {count} events  [source={source}]");
                if let Some(records) = resp["records"].as_array() {
                    for r in records.iter().take(20) {
                        println!(
                            "  node={} event={} value={} vtime={}",
                            r["node"].as_u64().unwrap_or(0),
                            r["event"].as_str().unwrap_or("?"),
                            r["value"].as_u64().unwrap_or(0),
                            r["vtime"].as_u64().unwrap_or(0),
                        );
                    }
                    if records.len() > 20 {
                        println!("  ... ({} more)", records.len() - 20);
                    }
                }
            }
        }
        TracingAction::Summary { run } => {
            let resp = c.get(&format!("/tracing/summary?run={run}")).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                let p1 = &resp["plane1"];
                let p2 = &resp["plane2"];
                println!("Tracing summary for run {run}");
                println!("  Plane 1 (supervisor syscall log): {} records",
                    p1["syscall_records"].as_u64().unwrap_or(0));
                println!("  Plane 2 ({}):", p2["source"].as_str().unwrap_or("fallback"));
                println!("    total events: {}", p2["total_events"].as_u64().unwrap_or(0));
                if let Some(ec) = p2["event_counts"].as_object() {
                    for (k, v) in ec {
                        println!("      {k}: {}", v.as_u64().unwrap_or(0));
                    }
                }
            }
        }
    }
    Ok(())
}
