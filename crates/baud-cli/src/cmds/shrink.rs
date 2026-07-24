use anyhow::Result;
use clap::Parser;
use crate::client::Client;

#[derive(Parser)]
pub struct ShrinkArgs {
    pub run: String,
    #[arg(long)] passes: Option<String>,
}

pub async fn run(_args: ShrinkArgs, _c: &Client, _json: bool) -> Result<()> {
    eprintln!("shrink: not yet implemented (M4+)");
    Ok(())
}
