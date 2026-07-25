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
    /// KVM host capability probe + regime decision (H0)
    Host(cmds::host::HostCmd),
    /// Guest-image contract checks (tape-device driver, no real RTC/HPET)
    Image(cmds::image::ImageCmd),
    /// Secrets management (alias: keys)
    Secrets(cmds::keys::KeysCmd),
    /// Secrets management (deprecated alias for 'secrets')
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
    /// Fuzz a workload (M4)
    Fuzz(cmds::fuzz::FuzzCmd),
    /// Budget accounting
    Budget,
    /// Restore a persisted universe into a live, interactive console session
    ShellInto(cmds::shell_into::ShellIntoArgs),
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("BAUD_LOG")
                .add_directive("baud=warn".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    let json = cli.json;
    let c = client::Client::new(&cli.server);

    let result: Result<()> = match cli.command {
        Commands::Server(cmd) => cmds::server::run(cmd, &c, json).await,
        Commands::Doctor => cmds::doctor::run(&c, json).await,
        Commands::Host(cmd) => cmds::host::run(cmd, &c, json).await,
        Commands::Image(cmd) => cmds::image::run(cmd, &c, json).await,
        Commands::Secrets(cmd) => cmds::keys::run(cmd, &c, json).await,
        Commands::Keys(cmd) => cmds::keys::run(cmd, &c, json).await,
        Commands::Spec(cmd) => cmds::spec::run(cmd, &c, json).await,
        Commands::Tape(cmd) => cmds::tape::run(cmd, &c, json).await,
        Commands::Run(cmd) => cmds::run::run(cmd, &c, json).await,
        Commands::Obs(cmd) => cmds::obs::run(cmd, &c, json).await,
        Commands::Syscalls(cmd) => cmds::syscalls::run(cmd, &c, json).await,
        Commands::Tracing(cmd) => cmds::tracing::run(cmd, &c, json).await,
        Commands::Net(cmd) => cmds::net::run(cmd, &c, json).await,
        Commands::Stream(cmd) => cmds::stream::run(cmd, &c, json).await,
        Commands::Verify(cmd) => cmds::verify::run(cmd, &c, json).await,
        Commands::Shrink(args) => cmds::shrink::run(args, &c, json).await,
        Commands::Replay(args) => cmds::replay::run(args, &c, json).await,
        Commands::Fuzz(cmd) => cmds::fuzz::run(cmd, &c, json).await,
        Commands::Budget => cmds::budget::run(&c, json).await,
        Commands::ShellInto(args) => cmds::shell_into::run(args, &c, json).await,
    };

    if let Err(e) = result {
        if json {
            // Machine-readable error output on stdout (baud-cli.md §4):
            // When --json is set, errors are emitted as {"ok": false, "error": "..."} to stdout.
            // This allows scripts to distinguish and parse error cases without inspecting stderr.
            let msg = format!("{e:#}");
            println!("{}", serde_json::json!({ "ok": false, "error": msg }));
        } else {
            eprintln!("error: {e:#}");
        }
        std::process::exit(1);
    }
}
