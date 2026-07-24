// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use crate::{client::Client, fmt};

/// Secrets management
#[derive(Parser)]
pub struct KeysCmd {
    #[command(subcommand)]
    pub action: KeysAction,
}

#[derive(Subcommand)]
pub enum KeysAction {
    /// Initialize a new secrets file
    Init {
        /// age recipient public key
        #[arg(long)]
        age_recipient: String,
    },
    /// Edit the secrets file
    Edit,
    /// Show key names (values always redacted)
    Show {
        #[arg(long)]
        redacted: bool,
    },
    /// Rotate the identity root key
    Rotate,
}

pub async fn run(cmd: KeysCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        KeysAction::Init { age_recipient } => {
            let v = c.post("/keys/init", &json!({ "age_recipient": age_recipient })).await?;
            fmt::print(&v, json);
        }
        KeysAction::Edit => {
            eprintln!("keys edit: open sops editor (not yet implemented)");
        }
        KeysAction::Show { .. } => {
            let v = c.get("/keys/show").await?;
            fmt::print(&v, json);
        }
        KeysAction::Rotate => {
            eprintln!("keys rotate: not yet implemented");
        }
    }
    Ok(())
}
