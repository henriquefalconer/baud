use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct TracingCmd {
    #[command(subcommand)]
    pub action: TracingAction,
}

#[derive(Subcommand)]
pub enum TracingAction {
    Tail { #[arg(long)] tape: String },
    Summary { #[arg(long)] run: String },
}

pub async fn run(_cmd: TracingCmd, _c: &Client, _json: bool) -> Result<()> {
    eprintln!("tracing: not yet implemented (M7+)");
    Ok(())
}
