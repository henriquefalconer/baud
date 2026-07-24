use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct VerifyCmd {
    #[command(subcommand)]
    pub action: VerifyAction,
}

#[derive(Subcommand)]
pub enum VerifyAction {
    Determinism {
        #[arg(long)] spec: String,
        #[arg(long)] seed: Option<u64>,
        #[arg(long, default_value = "2")] times: u32,
    },
    Observation {
        #[arg(long)] run: String,
    },
}

pub async fn run(_cmd: VerifyCmd, _c: &Client, _json: bool) -> Result<()> {
    eprintln!("verify: not yet implemented (M3+)");
    Ok(())
}
