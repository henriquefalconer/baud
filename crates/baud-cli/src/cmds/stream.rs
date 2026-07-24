use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct StreamCmd {
    #[command(subcommand)]
    pub action: StreamAction,
}

#[derive(Subcommand)]
pub enum StreamAction {
    Tail {
        #[arg(long)] run: String,
        #[arg(long)] node: Option<u16>,
        #[arg(short = 'o')] out: Option<String>,
        #[arg(long)] hashes_only: bool,
    },
    Render {
        #[arg(long)] run: String,
        #[arg(long)] from_step: Option<u64>,
        #[arg(long)] to_step: Option<u64>,
        #[arg(long, default_value = "y4m")] format: String,
        #[arg(short = 'o')] out: String,
    },
    Frames {
        #[arg(long)] run: String,
        #[arg(long)] node: Option<u16>,
    },
}

pub async fn run(_cmd: StreamCmd, _c: &Client, _json: bool) -> Result<()> {
    eprintln!("stream: not yet implemented (M5+)");
    Ok(())
}
