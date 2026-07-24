use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::client::Client;

#[derive(Parser)]
pub struct SpecCmd {
    #[command(subcommand)]
    pub action: SpecAction,
}

#[derive(Subcommand)]
pub enum SpecAction {
    /// Create a new spec file
    New { name: String },
    /// Lint a spec file
    Lint { path: String },
    /// Show a spec file
    Show { path: String },
}

pub async fn run(cmd: SpecCmd, _c: &Client, _json: bool) -> Result<()> {
    match cmd.action {
        SpecAction::New { name } => eprintln!("spec new {name}: not yet implemented"),
        SpecAction::Lint { path } => eprintln!("spec lint {path}: not yet implemented"),
        SpecAction::Show { path } => eprintln!("spec show {path}: not yet implemented"),
    }
    Ok(())
}
