use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct ObsCmd {
    #[command(subcommand)]
    pub action: ObsAction,
}

#[derive(Subcommand)]
pub enum ObsAction {
    Ls { #[arg(long)] run: String },
    Get { #[arg(long)] run: String, #[arg(long)] probe: Option<String> },
    Tail { #[arg(long)] run: String, #[arg(long)] probe: Option<String>, #[arg(long)] node: Option<u16> },
}

pub async fn run(_cmd: ObsCmd, _c: &Client, _json: bool) -> Result<()> {
    eprintln!("obs: not yet implemented (M3+)");
    Ok(())
}
