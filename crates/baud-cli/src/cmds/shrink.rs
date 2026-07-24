// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud shrink — shrink a crashed/completed run to the minimal tape reproducing the violation

use anyhow::Result;
use clap::Parser;
use serde_json::json;
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct ShrinkArgs {
    /// Run ID to shrink
    pub run: String,
    /// Comma-separated shrink passes (chunk-delete,zero,hold-shorten,dedup)
    #[arg(long)]
    pub passes: Option<String>,
}

pub async fn run(args: ShrinkArgs, c: &Client, json: bool) -> Result<()> {
    let body = json!({
        "passes": args.passes,
    });

    let v = c.post(&format!("/runs/{}/shrink", args.run), &body).await?;

    // Check exit code semantics: if shrunk successfully, print result
    if let Some(ok) = v.get("ok").and_then(|b| b.as_bool()) {
        if !ok {
            if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
                eprintln!("shrink error: {e}");
                std::process::exit(1);
            }
        }
    }

    fmt::print(&v, json);
    Ok(())
}
