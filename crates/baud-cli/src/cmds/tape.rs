use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct TapeCmd {
    #[command(subcommand)]
    pub action: TapeAction,
}

#[derive(Subcommand)]
pub enum TapeAction {
    Create,
    Ls,
    Status { id: String },
    Ensure { id: String },
    Kill { id: String },
    Reconstruct { id: String },
    Exec { id: String, cmd: Vec<String> },
    ProbeCaps { id: String },
}

pub async fn run(cmd: TapeCmd, _c: &Client, _json: bool) -> Result<()> {
    eprintln!("tape: not yet implemented (M1)");
    Ok(())
}
