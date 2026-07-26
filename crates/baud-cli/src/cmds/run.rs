// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud run — run management commands

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct RunCmd {
    #[command(subcommand)]
    pub action: RunAction,
}

#[derive(Subcommand)]
pub enum RunAction {
    /// Start a new run
    Start {
        /// Path to spec file (YAML)
        #[arg(long)]
        spec: String,
        /// Strategy spec (inline JSON or @path)
        #[arg(long)]
        strategy: Option<String>,
        /// Tactics spec (inline JSON or @path)
        #[arg(long)]
        tactics: Option<String>,
        /// RNG seed
        #[arg(long)]
        seed: Option<u64>,
        /// Budget in minutes
        #[arg(long)]
        budget_minutes: Option<u64>,
        /// Backend: "local" or "daytona"
        #[arg(long, default_value = "local")]
        backend: String,
    },
    /// List runs
    Ls,
    /// Show run status
    Status {
        /// Run ID
        run: String,
    },
    /// Watch a run (streaming output)
    Watch {
        /// Run ID
        run: String,
    },
    /// Pause a run
    Pause {
        /// Run ID
        run: String,
    },
    /// Resume a paused run
    Resume {
        /// Run ID
        run: String,
    },
    /// Abort a run
    Abort {
        /// Run ID
        run: String,
    },
    /// Boot a guest image directly on the real KVM Multiverse and run it to its first halt
    /// (H0-H6's post-pivot core — bypasses the sandbox/spec/tape machinery above entirely).
    Kvm {
        /// Path to a bzImage kernel on the server host's filesystem.
        #[arg(long)]
        kernel: String,
        /// Kernel command line. Omit to use the server's spec §4.2 deterministic default
        /// (`bootparams::DETERMINISTIC_CMDLINE`).
        #[arg(long)]
        cmdline: Option<String>,
        /// The run's whole tape, hex-encoded.
        #[arg(long, default_value = "")]
        tape_hex: String,
        /// Path to a reproducible initramfs on the server host's filesystem (e.g. the
        /// `initramfs.cpio.gz` `baud image build` writes) — booted alongside `--kernel`, spec
        /// §4.2/§4.3. Omit for a guest with no separate initramfs.
        #[arg(long)]
        initramfs: Option<String>,
        /// Work-clock period (retired conditional branches) between injected timer ticks — a real,
        /// unmodified Linux kernel guest's own scheduler calibration hangs forever without this
        /// (no hand-assembled fixture in this workspace needs it). Setting this enables periodic
        /// timer injection for the boot.
        #[arg(long)]
        periodic_timer_period_rcb: Option<u64>,
        /// Interrupt vector to inject at each tick. Defaults to `0xec`, Linux's own
        /// `LOCAL_TIMER_VECTOR`. Only used when `--periodic-timer-period-rcb` is set.
        #[arg(long, default_value_t = 0xec)]
        periodic_timer_vector: u8,
        /// Bound on ticks before giving up. Only used when `--periodic-timer-period-rcb` is set.
        #[arg(long, default_value_t = 2000)]
        periodic_timer_max_ticks: u32,
    },
    /// Boot a guest image, snapshot immediately after boot as a shared branch point, then fork one
    /// independent continuation per `--branch-tape-hex` (repeatable) — or, with `--generate-seed`
    /// and `--generate-count`, per a `baud-driver`-generated tape instead — and run each to its
    /// first halt: the KVM-era snapshot-tree exploration primitive (todo.md §5/§6).
    KvmBranch {
        /// Path to a bzImage kernel on the server host's filesystem.
        #[arg(long)]
        kernel: String,
        /// Kernel command line. Omit to use the server's spec §4.2 deterministic default
        /// (`bootparams::DETERMINISTIC_CMDLINE`).
        #[arg(long)]
        cmdline: Option<String>,
        /// A hex-encoded tape suffix for one branch. Repeat for multiple branches. Ignored when
        /// `--generate-seed`/`--generate-count` are set instead.
        #[arg(long = "branch-tape-hex", required_unless_present = "generate_seed")]
        branch_tapes_hex: Vec<String>,
        /// Persist the branch-point universe into the server's SnapshotStore under this run id —
        /// the response's `persisted.node_id` can later be handed to `kvm-resume` to fork more
        /// branches from the same point with no re-boot.
        #[arg(long)]
        persist_run_id: Option<String>,
        /// Generate branch tapes with `baud-driver` (seeded, reproducible) instead of supplying
        /// them via `--branch-tape-hex`. Requires `--generate-count`.
        #[arg(long, requires = "generate_count")]
        generate_seed: Option<u64>,
        /// Number of driver-generated branches to run.
        #[arg(long)]
        generate_count: Option<usize>,
        /// Bytes drawn per generated tape suffix.
        #[arg(long, default_value_t = 4)]
        generate_tape_len_bytes: u32,
        /// Probe name to maximize (`StrategySpec.maximize`), in priority order. Repeatable.
        #[arg(long = "maximize")]
        maximize: Vec<String>,
        /// Same flag as `kvm --initramfs`, applied to the boot that establishes this call's shared
        /// branch point. Omit for a guest with no separate initramfs.
        #[arg(long)]
        initramfs: Option<String>,
        /// Same flag as `kvm --periodic-timer-period-rcb`, applied to every branch this call
        /// forks. A real, unmodified Linux kernel guest's scheduler calibration hangs forever
        /// without this. Setting this enables periodic timer injection for every branch.
        #[arg(long)]
        periodic_timer_period_rcb: Option<u64>,
        /// Interrupt vector to inject at each tick. Defaults to `0xec`, Linux's own
        /// `LOCAL_TIMER_VECTOR`. Only used when `--periodic-timer-period-rcb` is set.
        #[arg(long, default_value_t = 0xec)]
        periodic_timer_vector: u8,
        /// Bound on ticks before giving up. Only used when `--periodic-timer-period-rcb` is set.
        #[arg(long, default_value_t = 2000)]
        periodic_timer_max_ticks: u32,
        /// A run id under which to persist one `--branch-tape-hex` branch's replay inputs and
        /// frames (`kvm_run_meta`/`frame_records`), so `baud stream render`/`baud stream frames`
        /// can later replay its real pixels instead of a synthetic gradient. Repeat once per
        /// `--branch-tape-hex`, in the same order; pass an empty string to skip persisting a given
        /// branch. Ignored when `--generate-seed`/`--generate-count` are set — use
        /// `--frame-run-id-prefix` instead.
        #[arg(long = "frame-run-id")]
        frame_run_ids: Vec<String>,
        /// Generate mode's analogue of `--frame-run-id`: persist every generated branch's frames
        /// under the run id `"{prefix}-{i}"` (`i` = the branch's 0-based index in this call).
        /// Ignored when `--branch-tape-hex` is set instead of `--generate-seed`/`--generate-count`.
        #[arg(long)]
        frame_run_id_prefix: Option<String>,
    },
    /// Fork more branches from a universe a prior `kvm-branch --persist-run-id` call persisted —
    /// no kernel image, no re-boot: reconstructs the universe from the server's SnapshotStore and
    /// runs each new branch to its first halt from there. `--generate-seed`/`--generate-count`
    /// drive `baud-driver` against the resumed universe instead of `--branch-tape-hex`, the
    /// symmetric follow-up to `kvm-branch`'s own generate mode.
    KvmResume {
        /// The `run_id` a prior `kvm-branch --persist-run-id` call persisted under.
        #[arg(long)]
        run_id: String,
        /// The `node_id` that same call returned in its `persisted.node_id` field.
        #[arg(long)]
        node_id: String,
        /// A hex-encoded tape suffix for one branch. Repeat for multiple branches. Ignored when
        /// `--generate-seed`/`--generate-count` are set instead.
        #[arg(long = "branch-tape-hex", required_unless_present = "generate_seed")]
        branch_tapes_hex: Vec<String>,
        /// Generate branch tapes with `baud-driver` (seeded, reproducible) instead of supplying
        /// them via `--branch-tape-hex`. Requires `--generate-count`.
        #[arg(long, requires = "generate_count")]
        generate_seed: Option<u64>,
        /// Number of driver-generated branches to run.
        #[arg(long)]
        generate_count: Option<usize>,
        /// Bytes drawn per generated tape suffix.
        #[arg(long, default_value_t = 4)]
        generate_tape_len_bytes: u32,
        /// Probe name to maximize (`StrategySpec.maximize`), in priority order. Repeatable.
        #[arg(long = "maximize")]
        maximize: Vec<String>,
        /// Same flag as `kvm --periodic-timer-period-rcb`, applied to every branch this call forks
        /// from the reconstructed universe. No `--initramfs` here: resuming never boots a kernel —
        /// the reconstructed universe alone is enough — but a resumed real-Linux-guest checkpoint
        /// still needs periodic ticks to make forward progress past it, exactly like a fresh
        /// branch does.
        #[arg(long)]
        periodic_timer_period_rcb: Option<u64>,
        /// Interrupt vector to inject at each tick. Defaults to `0xec`, Linux's own
        /// `LOCAL_TIMER_VECTOR`. Only used when `--periodic-timer-period-rcb` is set.
        #[arg(long, default_value_t = 0xec)]
        periodic_timer_vector: u8,
        /// Bound on ticks before giving up. Only used when `--periodic-timer-period-rcb` is set.
        #[arg(long, default_value_t = 2000)]
        periodic_timer_max_ticks: u32,
        /// A run id under which to persist one `--branch-tape-hex` branch's replay inputs and
        /// frames (`kvm_run_meta`/`frame_records`) — same convention as `kvm-branch
        /// --frame-run-id`, but persists a *restore*-based row (this call's `--run-id`/`--node-id`
        /// plus the branch's own tape suffix) instead of a reboot-based one, since resuming never
        /// boots a kernel. `baud stream render`/`baud stream frames` replay it via restore, not
        /// reboot. Repeat once per `--branch-tape-hex`, in the same order; pass an empty string to
        /// skip persisting a given branch. Ignored when `--generate-seed`/`--generate-count` are
        /// set — use `--frame-run-id-prefix` instead.
        #[arg(long = "frame-run-id")]
        frame_run_ids: Vec<String>,
        /// Generate mode's analogue of `--frame-run-id`: persist every generated branch's frames
        /// under the run id `"{prefix}-{i}"` (`i` = the branch's 0-based index in this call).
        /// Ignored when `--branch-tape-hex` is set instead of `--generate-seed`/`--generate-count`.
        #[arg(long)]
        frame_run_id_prefix: Option<String>,
    },
}

pub async fn run(cmd: RunCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        RunAction::Start {
            spec,
            strategy,
            tactics,
            seed,
            budget_minutes,
            backend,
        } => {
            let spec_content = std::fs::read_to_string(&spec)
                .map_err(|e| anyhow::anyhow!("failed to read spec '{}': {}", spec, e))?;
            let body = json!({
                "spec": spec_content,
                "strategy": strategy,
                "tactics": tactics,
                "seed": seed.unwrap_or(0),
                "budget_minutes": budget_minutes.unwrap_or(60),
                "backend": backend,
            });
            let v = c.post("/runs", &body).await?;
            fmt::print(&v, json);
            if v.get("error").is_some() {
                std::process::exit(1);
            }
        }
        RunAction::Ls => {
            let v = c.get("/runs").await?;
            fmt::print(&v, json);
        }
        RunAction::Status { run: id } => {
            let v = c.get(&format!("/runs/{id}")).await?;
            fmt::print(&v, json);
            if v.get("error").is_some() {
                std::process::exit(1);
            }
            // Exit code 2 when the run found a bug / goal (baud-cli.md §4 exit codes)
            let status_str = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if matches!(status_str, "crashed" | "goal" | "violation_found") {
                std::process::exit(2);
            }
        }
        RunAction::Abort { run: id } => {
            let v = c.post(&format!("/runs/{id}/abort"), &json!({})).await?;
            fmt::print(&v, json);
        }
        RunAction::Watch { run: id } => {
            // Stub: poll status (SSE in M3+)
            let v = c.get(&format!("/runs/{id}")).await?;
            fmt::print(&v, json);
        }
        RunAction::Pause { run: id } => {
            eprintln!("run pause {id}: not yet implemented (M4+)");
        }
        RunAction::Resume { run: id } => {
            eprintln!("run resume {id}: not yet implemented (M4+)");
        }
        RunAction::Kvm {
            kernel,
            cmdline,
            tape_hex,
            initramfs,
            periodic_timer_period_rcb,
            periodic_timer_vector,
            periodic_timer_max_ticks,
        } => {
            let mut body = json!({
                "kernel_path": kernel,
                "tape_hex": tape_hex,
                "initramfs_path": initramfs,
            });
            if let Some(cmdline) = cmdline {
                body["cmdline"] = json!(cmdline);
            }
            if let Some(period_rcb) = periodic_timer_period_rcb {
                body["periodic_timer"] = json!({
                    "period_rcb": period_rcb,
                    "vector": periodic_timer_vector,
                    "max_ticks": periodic_timer_max_ticks,
                });
            }
            let v = c.post("/run/kvm", &body).await?;
            fmt::print(&v, json);
            if v.get("error").is_some() {
                std::process::exit(1);
            }
        }
        RunAction::KvmBranch {
            kernel,
            cmdline,
            branch_tapes_hex,
            persist_run_id,
            generate_seed,
            generate_count,
            generate_tape_len_bytes,
            maximize,
            initramfs,
            periodic_timer_period_rcb,
            periodic_timer_vector,
            periodic_timer_max_ticks,
            frame_run_ids,
            frame_run_id_prefix,
        } => {
            let mut body = json!({
                "kernel_path": kernel,
                "persist_run_id": persist_run_id,
                "initramfs_path": initramfs,
            });
            if let Some(cmdline) = cmdline {
                body["cmdline"] = json!(cmdline);
            }
            if let Some(period_rcb) = periodic_timer_period_rcb {
                body["periodic_timer"] = json!({
                    "period_rcb": period_rcb,
                    "vector": periodic_timer_vector,
                    "max_ticks": periodic_timer_max_ticks,
                });
            }
            if let (Some(seed), Some(count)) = (generate_seed, generate_count) {
                body["generate"] = json!({
                    "seed": seed,
                    "count": count,
                    "tape_len_bytes": generate_tape_len_bytes,
                    "strategy": { "maximize": maximize },
                    "frame_run_id_prefix": frame_run_id_prefix,
                });
            } else {
                body["branch_tapes_hex"] = json!(branch_tapes_hex);
                if !frame_run_ids.is_empty() {
                    body["frame_run_ids"] = json!(frame_run_ids
                        .iter()
                        .map(|s| if s.is_empty() { serde_json::Value::Null } else { json!(s) })
                        .collect::<Vec<_>>());
                }
            }
            let v = c.post("/run/kvm/branch", &body).await?;
            fmt::print(&v, json);
            if v.get("error").is_some() {
                std::process::exit(1);
            }
        }
        RunAction::KvmResume {
            run_id,
            node_id,
            branch_tapes_hex,
            generate_seed,
            generate_count,
            generate_tape_len_bytes,
            maximize,
            periodic_timer_period_rcb,
            periodic_timer_vector,
            periodic_timer_max_ticks,
            frame_run_ids,
            frame_run_id_prefix,
        } => {
            let mut body = json!({
                "run_id": run_id,
                "node_id": node_id,
            });
            if let Some(period_rcb) = periodic_timer_period_rcb {
                body["periodic_timer"] = json!({
                    "period_rcb": period_rcb,
                    "vector": periodic_timer_vector,
                    "max_ticks": periodic_timer_max_ticks,
                });
            }
            if let (Some(seed), Some(count)) = (generate_seed, generate_count) {
                body["generate"] = json!({
                    "seed": seed,
                    "count": count,
                    "tape_len_bytes": generate_tape_len_bytes,
                    "strategy": { "maximize": maximize },
                    "frame_run_id_prefix": frame_run_id_prefix,
                });
            } else {
                body["branch_tapes_hex"] = json!(branch_tapes_hex);
                if !frame_run_ids.is_empty() {
                    body["frame_run_ids"] = json!(frame_run_ids
                        .iter()
                        .map(|s| if s.is_empty() { serde_json::Value::Null } else { json!(s) })
                        .collect::<Vec<_>>());
                }
            }
            let v = c.post("/run/kvm/resume", &body).await?;
            fmt::print(&v, json);
            if v.get("error").is_some() {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
