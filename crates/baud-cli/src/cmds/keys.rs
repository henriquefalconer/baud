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
        /// Path to plaintext template YAML (default: infra/secrets/baud.enc.yaml.example)
        #[arg(long)]
        template: Option<String>,
        /// Output path for encrypted file (default: infra/secrets/baud.enc.yaml)
        #[arg(long)]
        output: Option<String>,
    },
    /// Edit the secrets file interactively via sops
    Edit,
    /// Show key names (values always redacted)
    Show {
        #[arg(long)]
        redacted: bool,
    },
    /// Rotate the secrets file to a new age recipient (the previous recipient loses access)
    Rotate {
        /// age recipient public key to rotate to
        #[arg(long)]
        new_recipient: String,
    },
}

pub async fn run(cmd: KeysCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        KeysAction::Init { age_recipient, template, output } => {
            let v = c.post("/keys/init", &json!({
                "age_recipient": age_recipient,
                "template_path": template,
                "out_path": output,
            })).await?;
            fmt::print(&v, json);
        }
        KeysAction::Edit => {
            // baud keys edit delegates directly to sops (no server round-trip needed;
            // sops opens $EDITOR which requires a TTY, not an HTTP POST).
            use std::process::Command;
            let secrets = baud_keys::secrets_file();
            let key_path = baud_keys::age_key_path();
            let mut cmd = Command::new("sops");
            cmd.arg(&secrets);
            if let Some(kp) = &key_path {
                cmd.env("SOPS_AGE_KEY_FILE", kp);
            }
            let status = cmd.status()?;
            if !status.success() {
                anyhow::bail!("sops exited with status {status}");
            }
        }
        KeysAction::Show { .. } => {
            let v = c.get("/keys/show").await?;
            fmt::print(&v, json);
        }
        KeysAction::Rotate { new_recipient } => {
            let v = c.post("/keys/rotate", &json!({ "new_recipient": new_recipient })).await?;
            fmt::print(&v, json);
        }
    }
    Ok(())
}
