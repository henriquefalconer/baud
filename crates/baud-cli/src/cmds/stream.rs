// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud stream — frame streaming commands (M5)

use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct StreamCmd {
    #[command(subcommand)]
    pub action: StreamAction,
}

#[derive(Subcommand)]
pub enum StreamAction {
    /// List journaled frame hashes for a run (by step)
    Frames {
        #[arg(long)]
        run: String,
        #[arg(long)]
        node: Option<u16>,
        #[arg(long)]
        from_step: Option<u64>,
        #[arg(long)]
        to_step: Option<u64>,
    },
    /// Tail live frames over SSE (or list stored frames)
    Tail {
        #[arg(long)]
        run: String,
        #[arg(long)]
        node: Option<u16>,
        #[arg(short = 'o')]
        out: Option<String>,
        #[arg(long)]
        hashes_only: bool,
    },
    /// Replay with capture: materialise frames from the tape
    Render {
        #[arg(long)]
        run: String,
        #[arg(long)]
        from_step: Option<u64>,
        #[arg(long)]
        to_step: Option<u64>,
        #[arg(long, default_value = "y4m")]
        format: String,
        #[arg(short = 'o')]
        out: String,
    },
}

pub async fn run(cmd: StreamCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        StreamAction::Frames { run, node, from_step, to_step } => {
            let mut url = format!("/runs/{run}/frames");
            let mut params = Vec::new();
            if let Some(n) = node { params.push(format!("node={n}")); }
            if let Some(s) = from_step { params.push(format!("from_step={s}")); }
            if let Some(s) = to_step { params.push(format!("to_step={s}")); }
            if !params.is_empty() {
                url = format!("{url}?{}", params.join("&"));
            }
            let resp: serde_json::Value = c.get(&url).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                let frames = resp["frames"].as_array().cloned().unwrap_or_default();
                if frames.is_empty() {
                    println!("No frames recorded for run {run}");
                } else {
                    println!("{:<8} {:<6} {:<12}", "step", "node", "hash");
                    println!("{}", "-".repeat(30));
                    for f in &frames {
                        let step = f["step"].as_i64().unwrap_or(0);
                        let node_id = f["node"].as_i64().unwrap_or(0);
                        let hash = f["hash"].as_str().unwrap_or("?");
                        println!("{:<8} {:<6} {}", step, node_id, &hash[..16.min(hash.len())]);
                    }
                    println!("  {} frame(s) total", frames.len());
                }
            }
        }

        StreamAction::Tail { run, node, out, hashes_only } => {
            let mut url = format!("/runs/{run}/stream/tail");
            let mut params = Vec::new();
            if let Some(n) = node { params.push(format!("node={n}")); }
            if hashes_only { params.push("hashes_only=true".to_string()); }
            if !params.is_empty() {
                url = format!("{url}?{}", params.join("&"));
            }
            let resp: serde_json::Value = c.get(&url).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                let frames = resp["frames"].as_array().cloned().unwrap_or_default();
                println!("stream tail — run={run} ({} frames)", frames.len());
                if let Some(o) = out {
                    println!("  (would write Y4M to {o} — replay not yet implemented)");
                }
                for f in &frames {
                    let step = f["step"].as_i64().unwrap_or(0);
                    let hash = f["hash"].as_str().unwrap_or("?");
                    if hashes_only {
                        println!("  step={step} hash={}", &hash[..16.min(hash.len())]);
                    } else {
                        let w = f["width"].as_i64().unwrap_or(0);
                        let h = f["height"].as_i64().unwrap_or(0);
                        let fmt = f["format"].as_str().unwrap_or("?");
                        println!("  step={step} {w}x{h} {fmt} hash={}", &hash[..16.min(hash.len())]);
                    }
                }
            }
        }

        StreamAction::Render { run, from_step, to_step, format, out } => {
            let body = serde_json::json!({
                "from_step": from_step,
                "to_step": to_step,
                "format": format,
                "out": out,
            });
            let resp: serde_json::Value = c.post(&format!("/runs/{run}/stream/render"), &body).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                if resp["error"].is_string() {
                    eprintln!("render error: {}", resp["error"].as_str().unwrap_or("?"));
                } else {
                    let count = resp["frame_count"].as_u64().unwrap_or(0);
                    let w = resp["width"].as_i64().unwrap_or(0);
                    let h = resp["height"].as_i64().unwrap_or(0);
                    let out_path = resp["out"].as_str().unwrap_or("?");
                    println!("render: {count} frames ({w}x{h}) → {out_path} [{format}]");
                }
            }
        }
    }
    Ok(())
}
