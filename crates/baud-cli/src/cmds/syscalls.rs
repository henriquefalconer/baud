// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud syscalls — supervisor syscall log (plane 1)  (M7)
//
// baud syscalls tail --run <id> [--node <n>] [--sysno <N>]
// baud syscalls get  --run <id> [--node <n>] [--sysno <N>]

use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct SyscallsCmd {
    #[command(subcommand)]
    pub action: SyscallsAction,
}

#[derive(Subcommand)]
pub enum SyscallsAction {
    /// Stream last 100 syscall records
    Tail {
        #[arg(long)]
        run: String,
        #[arg(long)]
        node: Option<u16>,
        #[arg(long)]
        sysno: Option<u32>,
    },
    /// List all syscall records
    Get {
        #[arg(long)]
        run: String,
        #[arg(long)]
        node: Option<u16>,
        #[arg(long)]
        sysno: Option<u32>,
    },
}

pub async fn run(cmd: SyscallsCmd, c: &Client, json: bool) -> Result<()> {
    let (path_prefix, run, node, sysno) = match &cmd.action {
        SyscallsAction::Tail { run, node, sysno } => ("tail", run, node, sysno),
        SyscallsAction::Get { run, node, sysno } => ("", run, node, sysno),
    };

    let endpoint = if path_prefix.is_empty() {
        format!("/runs/{run}/syscalls")
    } else {
        format!("/runs/{run}/syscalls/{path_prefix}")
    };

    let mut url = endpoint;
    let mut first = true;
    if let Some(n) = node {
        url.push_str(&format!("{}node={n}", if first { "?" } else { "&" }));
        first = false;
    }
    if let Some(s) = sysno {
        url.push_str(&format!("{}sysno={s}", if first { "?" } else { "&" }));
    }

    let resp = c.get(&url).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        let count = resp["count"].as_u64().unwrap_or(0);
        println!("syscalls for run {run}: {count} records");
        if let Some(records) = resp["records"].as_array() {
            for r in records.iter().take(30) {
                println!(
                    "  node={} sysno={} ret={} vtime={}",
                    r["node"].as_u64().unwrap_or(0),
                    r["sysno"].as_u64().unwrap_or(0),
                    r["ret"].as_i64().unwrap_or(0),
                    r["vtime"].as_u64().unwrap_or(0),
                );
            }
            if records.len() > 30 {
                println!("  ... ({} more)", records.len() - 30);
            }
        }
    }
    Ok(())
}
