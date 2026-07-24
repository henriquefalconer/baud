// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud host — capability probe + regime decision (specs/baud-host.md, milestone H0)

use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct HostCmd {
    #[command(subcommand)]
    pub action: HostAction,
}

#[derive(Subcommand)]
pub enum HostAction {
    /// Probe this host's KVM/VT-x capabilities and report the determinism regime it supports.
    Probe,
}

pub async fn run(cmd: HostCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        HostAction::Probe => {
            let v = c.get("/host/probe").await?;
            fmt::print(&v, json);
            // A required capability failed: this host cannot run baud at all (specs/baud-host.md
            // §4). Never a false pass — exit 1 so scripts/drives notice.
            if v.get("regime").and_then(|r| r.as_str()) == Some("rejected") {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
