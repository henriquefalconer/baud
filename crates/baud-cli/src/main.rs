// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud — the single CLI binary
//
// Thin client: zero business logic. Each subcommand = one server call + one formatter.
// Global: --json on every command.

mod client;
mod cmds;
mod fmt;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// baud: deterministic-validation infrastructure for distributed systems.
#[derive(Parser)]
#[command(name = "baud", version, about)]
pub struct Cli {
    /// Server address (default: http://127.0.0.1:7734)
    #[arg(long, env = "BAUD_SERVER", default_value = "http://127.0.0.1:7734")]
    pub server: String,

    /// Output raw JSON (no table formatting)
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Server lifecycle management
    Server(cmds::server::ServerCmd),
    /// Environment checks
    Doctor,
    /// Secrets management
    Keys(cmds::keys::KeysCmd),
    /// Spec management
    Spec(cmds::spec::SpecCmd),
    /// Tape (sandbox) lifecycle
    Tape(cmds::tape::TapeCmd),
    /// Run management
    Run(cmds::run::RunCmd),
    /// Observation access
    Obs(cmds::obs::ObsCmd),
    /// Syscall log access
    Syscalls(cmds::syscalls::SyscallsCmd),
    /// Tracing
    Tracing(cmds::tracing::TracingCmd),
    /// Network weather
    Net(cmds::net::NetCmd),
    /// Frame stream
    Stream(cmds::stream::StreamCmd),
    /// Determinism verification
    Verify(cmds::verify::VerifyCmd),
    /// Shrink a run to minimal tape
    Shrink(cmds::shrink::ShrinkArgs),
    /// Replay a run
    Replay(cmds::replay::ReplayArgs),
    /// Budget accounting
    Budget,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("BAUD_LOG")
                .add_directive("baud=warn".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    let c = client::Client::new(&cli.server);

    match cli.command {
        Commands::Server(cmd) => cmds::server::run(cmd, &c, cli.json).await,
        Commands::Doctor => cmds::doctor::run(&c, cli.json).await,
        Commands::Keys(cmd) => cmds::keys::run(cmd, &c, cli.json).await,
        Commands::Spec(cmd) => cmds::spec::run(cmd, &c, cli.json).await,
        Commands::Tape(cmd) => cmds::tape::run(cmd, &c, cli.json).await,
        Commands::Run(cmd) => cmds::run::run(cmd, &c, cli.json).await,
        Commands::Obs(cmd) => cmds::obs::run(cmd, &c, cli.json).await,
        Commands::Syscalls(cmd) => cmds::syscalls::run(cmd, &c, cli.json).await,
        Commands::Tracing(cmd) => cmds::tracing::run(cmd, &c, cli.json).await,
        Commands::Net(cmd) => cmds::net::run(cmd, &c, cli.json).await,
        Commands::Stream(cmd) => cmds::stream::run(cmd, &c, cli.json).await,
        Commands::Verify(cmd) => cmds::verify::run(cmd, &c, cli.json).await,
        Commands::Shrink(args) => cmds::shrink::run(args, &c, cli.json).await,
        Commands::Replay(args) => cmds::replay::run(args, &c, cli.json).await,
        Commands::Budget => cmds::budget::run(&c, cli.json).await,
    }
}
