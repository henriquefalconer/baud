use anyhow::Result;
use clap::Parser;
use crate::client::Client;

#[derive(Parser)]
pub struct ReplayArgs {
    pub run: String,
    #[arg(long)] tape_file: Option<String>,
    #[arg(long)] to_step: Option<u64>,
}

pub async fn run(_args: ReplayArgs, _c: &Client, _json: bool) -> Result<()> {
    eprintln!("replay: not yet implemented (M3+)");
    Ok(())
}
