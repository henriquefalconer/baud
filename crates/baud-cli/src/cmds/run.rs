use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct RunCmd {
    #[command(subcommand)]
    pub action: RunAction,
}

#[derive(Subcommand)]
pub enum RunAction {
    Start {
        #[arg(long)] spec: String,
        #[arg(long)] strategy: Option<String>,
        #[arg(long)] tactics: Option<String>,
        #[arg(long)] seed: Option<u64>,
        #[arg(long)] budget_minutes: Option<u64>,
    },
    Ls,
    Status { run: String },
    Watch { run: String },
    Pause { run: String },
    Resume { run: String },
    Abort { run: String },
}

pub async fn run(cmd: RunCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        RunAction::Ls => {
            let v = c.get("/runs").await?;
            fmt::print(&v, json);
        }
        _ => eprintln!("run: not yet implemented (M2+)"),
    }
    Ok(())
}
