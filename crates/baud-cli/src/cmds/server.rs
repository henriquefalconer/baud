// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crate::{client::Client, fmt};

/// PID file location for the baud-server process.
fn pid_file() -> std::path::PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(".baud");
    dir.join("server.pid")
}

/// Is a process with this pid still alive? Unix: `kill(pid, 0)` (signal 0 probes without
/// sending). Windows has no such libc call — shell out to `tasklist`, which is present on every
/// Windows install (no extra dependency needed for a rarely-hot-path check).
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

/// Terminate a process by pid. Unix: `SIGTERM`. Windows: `taskkill /F` (no graceful-shutdown
/// signal equivalent to SIGTERM is available without WinAPI, so this is a hard kill).
#[cfg(unix)]
fn terminate_process(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> bool {
    std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Manage the baud-server process
#[derive(Parser)]
pub struct ServerCmd {
    #[command(subcommand)]
    pub action: ServerAction,
}

#[derive(Subcommand)]
pub enum ServerAction {
    /// Start the server (prints the address it listens on)
    Start,
    /// Stop the server
    Stop,
    /// Show server status
    Status,
    /// Show server logs
    Logs {
        /// Follow new log lines
        #[arg(long, short = 'f')]
        follow: bool,
    },
}

pub async fn run(cmd: ServerCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        ServerAction::Start => {
            // Spawn baud-server binary as a background process.
            // baud-server is expected to be on PATH or in the same directory as baud.
            let pid_path = pid_file();

            // Check if already running
            if pid_path.exists() {
                if let Ok(s) = std::fs::read_to_string(&pid_path) {
                    if let Ok(pid) = s.trim().parse::<u32>() {
                        // Check if the process is still alive
                        if process_is_alive(pid) {
                            if json {
                                println!("{}", serde_json::json!({ "status": "already_running", "pid": pid }));
                            } else {
                                println!("baud-server is already running (pid {pid})");
                            }
                            return Ok(());
                        }
                    }
                }
            }

            // Ensure PID file directory exists
            if let Some(parent) = pid_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Find baud-server binary
            let server_bin = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("baud-server")))
                .filter(|p| p.exists())
                .unwrap_or_else(|| std::path::PathBuf::from("baud-server"));

            let child = std::process::Command::new(&server_bin)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();

            match child {
                Ok(child) => {
                    let pid = child.id();
                    std::fs::write(&pid_path, pid.to_string())?;
                    if json {
                        println!("{}", serde_json::json!({ "status": "started", "pid": pid, "pid_file": pid_path.to_string_lossy() }));
                    } else {
                        println!("baud-server started (pid {pid}), listening on http://127.0.0.1:7734");
                    }
                }
                Err(e) => {
                    bail!("failed to start baud-server ({server_bin:?}): {e}");
                }
            }
            Ok(())
        }
        ServerAction::Stop => {
            let pid_path = pid_file();
            if !pid_path.exists() {
                if json {
                    println!("{}", serde_json::json!({ "status": "not_running" }));
                } else {
                    println!("baud-server is not running (no pid file at {:?})", pid_path);
                }
                return Ok(());
            }

            let s = std::fs::read_to_string(&pid_path)?;
            let pid: u32 = s.trim().parse().map_err(|_| anyhow::anyhow!("invalid pid in {:?}", pid_path))?;

            // Terminate the process (SIGTERM on Unix, taskkill /F on Windows).
            if terminate_process(pid) {
                std::fs::remove_file(&pid_path).ok();
                if json {
                    println!("{}", serde_json::json!({ "status": "stopped", "pid": pid }));
                } else {
                    println!("baud-server (pid {pid}) sent SIGTERM");
                }
            } else {
                // Process not found — remove stale pid file
                std::fs::remove_file(&pid_path).ok();
                if json {
                    println!("{}", serde_json::json!({ "status": "not_running", "note": "stale pid file removed" }));
                } else {
                    println!("baud-server process {pid} not found (stale pid file removed)");
                }
            }
            Ok(())
        }
        ServerAction::Status => {
            let v = c.get("/server/status").await?;
            fmt::print(&v, json);
            Ok(())
        }
        ServerAction::Logs { follow } => {
            // Fetch initial batch of logs
            let v = c.get("/server/logs").await?;
            let mut last_seq: u64 = v
                .get("last_seq")
                .and_then(|s| s.as_u64())
                .unwrap_or(0);

            // Print initial logs
            if let Some(logs) = v.get("logs").and_then(|l| l.as_array()) {
                for entry in logs {
                    let ts = entry.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
                    let level = entry.get("level").and_then(|l| l.as_str()).unwrap_or("INFO");
                    let msg = entry.get("msg").and_then(|m| m.as_str()).unwrap_or("");
                    if json {
                        println!("{entry}");
                    } else {
                        println!("[{ts}] {level}: {msg}");
                    }
                }
            }

            if !follow {
                return Ok(());
            }

            // --follow: poll for new entries every 500 ms until interrupted
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                let url = format!("/server/logs?after={last_seq}");
                let v = match c.get(&url).await {
                    Ok(v) => v,
                    Err(_) => continue, // server may be restarting
                };
                let new_seq = v
                    .get("last_seq")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(last_seq);

                if let Some(logs) = v.get("logs").and_then(|l| l.as_array()) {
                    for entry in logs {
                        let ts = entry.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
                        let level = entry.get("level").and_then(|l| l.as_str()).unwrap_or("INFO");
                        let msg = entry.get("msg").and_then(|m| m.as_str()).unwrap_or("");
                        if json {
                            println!("{entry}");
                        } else {
                            println!("[{ts}] {level}: {msg}");
                        }
                    }
                }
                last_seq = new_seq;
            }
        }
    }
}
