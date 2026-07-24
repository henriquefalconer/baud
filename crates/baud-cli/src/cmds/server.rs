// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::{client::Client, fmt};

/// Manage the baud-server process
#[derive(Parser)]
pub struct ServerCmd {
    #[command(subcommand)]
    pub action: ServerAction,
}

#[derive(Subcommand)]
pub enum ServerAction {
    /// Start the server (prints the address it listens on)
    Start,
    /// Stop the server
    Stop,
    /// Show server status
    Status,
    /// Show server logs
    Logs {
        /// Follow new log lines
        #[arg(long, short = 'f')]
        follow: bool,
    },
}

pub async fn run(cmd: ServerCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        ServerAction::Start => {
            eprintln!("start: launch baud-server binary directly (e.g. cargo run -p baud-server)");
            Ok(())
        }
        ServerAction::Stop => {
            eprintln!("stop: send SIGTERM to the baud-server process");
            Ok(())
        }
        ServerAction::Status => {
            let v = c.get("/server/status").await?;
            fmt::print(&v, json);
            Ok(())
        }
        ServerAction::Logs { follow: _ } => {
            let v = c.get("/server/logs").await?;
            fmt::print(&v, json);
            Ok(())
        }
    }
}
