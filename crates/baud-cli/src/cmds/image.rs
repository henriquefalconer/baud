// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud image — guest-image contract checks (todo.md §4, specs/baud-packages.md §9)

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct ImageCmd {
    #[command(subcommand)]
    pub action: ImageAction,
}

#[derive(Subcommand)]
pub enum ImageAction {
    /// Lint a guest kernel .config against the tape-device image contract: the tape-device driver
    /// must be enabled and no real hardware timer (RTC/HPET) baud does not model may be enabled.
    Lint {
        /// Path to the guest kernel's `.config` file.
        path: String,
    },
}

pub async fn run(cmd: ImageCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        ImageAction::Lint { path } => {
            let content = std::fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("failed to read kernel config '{}': {}", path, e)
            })?;
            let body = json!({ "content": content });
            let v = c.post("/image/lint", &body).await?;
            let ok = v.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            fmt::print(&v, json);
            // Never a false pass: a contract violation exits 1 so scripts/drives notice
            // (mirrors `baud host probe`'s rejected-regime handling and `baud spec lint`'s
            // not-ok handling).
            if !ok {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
