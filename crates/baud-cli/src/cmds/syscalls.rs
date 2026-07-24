use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct SyscallsCmd {
    #[command(subcommand)]
    pub action: SyscallsAction,
}

#[derive(Subcommand)]
pub enum SyscallsAction {
    Tail { #[arg(long)] run: String, #[arg(long)] node: Option<u16>, #[arg(long)] sysno: Option<u32> },
    Get { #[arg(long)] run: String },
}

pub async fn run(_cmd: SyscallsCmd, _c: &Client, _json: bool) -> Result<()> {
    eprintln!("syscalls: not yet implemented (M3+)");
    Ok(())
}
