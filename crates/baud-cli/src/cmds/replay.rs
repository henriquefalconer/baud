// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud replay — replay a run from its stored tape (M3)

use anyhow::Result;
use clap::Parser;
use serde_json::json;
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct ReplayArgs {
    /// Run ID to replay
    pub run: String,
    /// Optional tape file (CBOR-encoded tape bytes)
    #[arg(long)]
    pub tape_file: Option<String>,
    /// Replay up to this step (inclusive)
    #[arg(long)]
    pub to_step: Option<u64>,
}

pub async fn run(args: ReplayArgs, c: &Client, json: bool) -> Result<()> {
    // Optionally read tape file
    let tape_bytes = if let Some(path) = &args.tape_file {
        Some(std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("failed to read tape file {path}: {e}"))?)
    } else {
        None
    };

    let body = json!({
        "tape_bytes": tape_bytes,
        "to_step": args.to_step,
    });

    let v = c.post(&format!("/replay/{}", args.run), &body).await?;
    fmt::print(&v, json);

    // Exit 1 if not ok
    if !v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
        std::process::exit(1);
    }

    Ok(())
}
