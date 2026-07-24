// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud obs — observation access commands

use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct ObsCmd {
    #[command(subcommand)]
    pub action: ObsAction,
}

#[derive(Subcommand)]
pub enum ObsAction {
    /// List observations for a run
    Ls {
        #[arg(long)] run: String,
        #[arg(long)] probe: Option<String>,
        #[arg(long)] node: Option<u16>,
    },
    /// Get a specific observation
    Get {
        #[arg(long)] run: String,
        #[arg(long)] probe: Option<String>,
    },
    /// Tail observations (streaming)
    Tail {
        #[arg(long)] run: String,
        #[arg(long)] probe: Option<String>,
        #[arg(long)] node: Option<u16>,
    },
}

pub async fn run(cmd: ObsCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        ObsAction::Ls { run, probe, node } => {
            let mut url = format!("/runs/{run}/obs");
            let mut params = Vec::new();
            if let Some(p) = probe { params.push(format!("probe={p}")); }
            if let Some(n) = node { params.push(format!("node={n}")); }
            if !params.is_empty() { url.push('?'); url.push_str(&params.join("&")); }
            let v = c.get(&url).await?;
            fmt::print(&v, json);
        }
        ObsAction::Get { run, probe } => {
            let mut url = format!("/runs/{run}/obs");
            if let Some(p) = probe { url.push_str(&format!("?probe={p}")); }
            let v = c.get(&url).await?;
            fmt::print(&v, json);
        }
        ObsAction::Tail { run, probe: _, node: _ } => {
            // Stub: returns current observations (SSE in M3+)
            let v = c.get(&format!("/runs/{run}/obs/tail")).await?;
            fmt::print(&v, json);
        }
    }
    Ok(())
}
