use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct NetCmd {
    #[command(subcommand)]
    pub action: NetAction,
}

#[derive(Subcommand)]
pub enum NetAction {
    Weather { #[arg(long)] run: String },
}

pub async fn run(_cmd: NetCmd, _c: &Client, _json: bool) -> Result<()> {
    eprintln!("net: not yet implemented (M5+)");
    Ok(())
}
