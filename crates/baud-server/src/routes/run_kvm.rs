// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// POST /run/kvm — boot a guest image on the real, post-pivot KVM `Multiverse`
// (baud_multiverse::linux::Multiverse, proven deterministic on real hardware through H0-H6) and
// run it to its first halt.
//
// This is the first `baud-server` route that calls into that module at all: `/verify/determinism`
// and `/replay/:id` still construct the pre-pivot, userspace-simulation `Multiverse` from
// `baud_multiverse::lib.rs` (todo.md §14's "every existing route still imports the old pre-pivot
// Multiverse" gap, confirmed by grep before this route was added). Linux-only, like the module it
// calls (`baud_multiverse::linux` is itself `#[cfg(target_os = "linux")]`) — this workspace only
// ever builds/runs on real Linux+KVM hosts (CLAUDE.md), so there is no non-Linux fallback to write.

use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct RunKvmBody {
    /// Path to a bzImage kernel on this host's filesystem.
    pub kernel_path: String,
    /// Kernel command line. Defaults to the console-only line every fixture in this workspace uses.
    #[serde(default = "default_cmdline")]
    pub cmdline: String,
    /// The run's whole tape, hex-encoded (empty tape if omitted — a guest that never reads the
    /// tape device runs the same either way).
    #[serde(default)]
    pub tape_hex: String,
}

fn default_cmdline() -> String {
    "console=ttyS0".to_owned()
}

/// POST /run/kvm — boot `kernel_path` and run it to its first `Hlt`/`Shutdown`.
pub async fn run(Json(body): Json<RunKvmBody>) -> Json<Value> {
    let tape = match hex_decode(&body.tape_hex) {
        Some(t) => t,
        None => return Json(json!({ "error": "tape_hex must be a valid hex string" })),
    };
    let kernel_path = PathBuf::from(&body.kernel_path);
    let cmdline = body.cmdline;

    // Real ioctls (KVM_RUN and friends) block; keep them off the async executor.
    let result = tokio::task::spawn_blocking(move || boot_and_run(&kernel_path, &cmdline, tape))
        .await
        .expect("run/kvm task panicked");

    match result {
        Ok((console_output, ram_hash)) => Json(json!({
            "ok": true,
            "console_output_hex": hex_encode(&console_output),
            "ram_hash": ram_hash,
        })),
        Err(e) => Json(json!({ "error": e })),
    }
}

fn boot_and_run(kernel_path: &Path, cmdline: &str, tape: Vec<u8>) -> Result<(Vec<u8>, String), String> {
    let mut mv = baud_multiverse::linux::Multiverse::boot(kernel_path, cmdline, 0, 1, tape, None)
        .map_err(|e| format!("boot error: {e}"))?;
    let outcome = mv.run_to_first_halt().map_err(|e| format!("determinism hole: {e}"))?;
    Ok((outcome.console_output, outcome.ram_hash))
}

/// The work-clock constant this route uses for every boot/branch — a run-level constant
/// (`virtual_tsc = base + k * rcb`, `Multiverse::restore`'s doc), not part of captured state, so
/// every branch of the same request must share the value the branch point was booted with.
const WORK_CLOCK_K: u64 = 1;

/// Real per-branch cost is one full `KVM_CREATE_VM`/vCPU/guest-RAM-region lifecycle
/// (`Multiverse::branch`'s doc — the spec's documented small-N `fork()` fallback, not yet the
/// O(write-set) `UFFDIO_CONTINUE` sharing todo.md §14 tracks as still open), so an unbounded
/// branch count turns one HTTP request into an arbitrarily long blocking call. This caps a single
/// request at a size that stays well within normal request-timeout budgets on this dev host
/// (~200ms/branch measured by `thousand_branches_are_independent_and_deterministic`).
const MAX_BRANCHES_PER_REQUEST: usize = 256;

#[derive(Debug, Deserialize)]
pub struct RunKvmBranchBody {
    /// Path to a bzImage kernel on this host's filesystem — booted once to establish the branch
    /// point (a snapshot taken immediately after boot, before any guest instruction runs, mirroring
    /// `thousand_branches_are_independent_and_deterministic`'s own branch point).
    pub kernel_path: String,
    /// Kernel command line. Defaults to the console-only line every fixture in this workspace uses.
    #[serde(default = "default_cmdline")]
    pub cmdline: String,
    /// One hex-encoded tape suffix per branch — each is forked independently from the shared branch
    /// point via `Multiverse::branch` and run to its first halt.
    pub branch_tapes_hex: Vec<String>,
}

/// POST /run/kvm/branch — boot `kernel_path`, snapshot immediately after boot as the shared branch
/// point, then fork one independent `Multiverse` continuation per entry in `branch_tapes_hex`
/// (`Multiverse::branch`, specs/baud-snapshot.md §4's `Snapshot::branch`) and run each to its first
/// halt. No branch observes another's state — the same guarantee
/// `thousand_branches_are_independent_and_deterministic` proves at the crate level, exposed here as
/// the M-series' first real snapshot-tree-exploration server route (todo.md §14's "Natural next
/// steps" for `/run/kvm`).
pub async fn branch(Json(body): Json<RunKvmBranchBody>) -> Json<Value> {
    if body.branch_tapes_hex.is_empty() {
        return Json(json!({ "error": "branch_tapes_hex must contain at least one tape" }));
    }
    if body.branch_tapes_hex.len() > MAX_BRANCHES_PER_REQUEST {
        return Json(json!({
            "error": format!(
                "too many branches requested ({}) — max {MAX_BRANCHES_PER_REQUEST} per call",
                body.branch_tapes_hex.len()
            )
        }));
    }
    let mut tape_suffixes = Vec::with_capacity(body.branch_tapes_hex.len());
    for hex in &body.branch_tapes_hex {
        match hex_decode(hex) {
            Some(bytes) => tape_suffixes.push(bytes),
            None => return Json(json!({ "error": "branch_tapes_hex must contain only valid hex strings" })),
        }
    }
    let kernel_path = PathBuf::from(&body.kernel_path);
    let cmdline = body.cmdline;

    let result =
        tokio::task::spawn_blocking(move || boot_snapshot_and_branch(&kernel_path, &cmdline, tape_suffixes))
            .await
            .expect("run/kvm/branch task panicked");

    match result {
        Ok(outcomes) => {
            let branches: Vec<Value> = outcomes
                .into_iter()
                .map(|(console_output, ram_hash)| {
                    json!({ "console_output_hex": hex_encode(&console_output), "ram_hash": ram_hash })
                })
                .collect();
            Json(json!({ "ok": true, "branches": branches }))
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

fn boot_snapshot_and_branch(
    kernel_path: &Path,
    cmdline: &str,
    tape_suffixes: Vec<Vec<u8>>,
) -> Result<Vec<(Vec<u8>, String)>, String> {
    let mut boot = baud_multiverse::linux::Multiverse::boot(kernel_path, cmdline, 0, WORK_CLOCK_K, vec![], None)
        .map_err(|e| format!("boot error: {e}"))?;
    let mut page_store = baud_snapshot::PageStore::new();
    let universe = boot
        .snapshot(&mut page_store)
        .map_err(|e| format!("snapshot error: {e}"))?;

    let mut outcomes = Vec::with_capacity(tape_suffixes.len());
    for (i, suffix) in tape_suffixes.into_iter().enumerate() {
        let mut branch = baud_multiverse::linux::Multiverse::branch(&universe, suffix, WORK_CLOCK_K, None)
            .map_err(|e| format!("branch {i} error: {e}"))?;
        let outcome = branch
            .run_to_first_halt()
            .map_err(|e| format!("branch {i} determinism hole: {e}"))?;
        outcomes.push((outcome.console_output, outcome.ram_hash));
    }
    Ok(outcomes)
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() {
        return Some(Vec::new());
    }
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_guest_kernel_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/hello-guest/bzImage")
    }

    fn tape_echo_guest_kernel_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/tape-echo-guest/bzImage")
    }

    /// Server-level analogue of `baud-multiverse`'s own `double_boot_memory_identical`
    /// (specs/baud-multiverse.md §3.1): booting the same image+tape twice through this route's own
    /// `boot_and_run` (the exact function the HTTP handler calls, minus only the axum/JSON
    /// plumbing) must yield byte-identical console output and RAM hash. Confirms this route wires
    /// the real KVM `Multiverse` correctly, not just that the crate underneath it is deterministic.
    #[test]
    fn run_kvm_boot_is_deterministic() {
        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let (first_console, first_hash) =
            boot_and_run(&kernel, cmdline, vec![]).expect("first boot failed");
        let (second_console, second_hash) =
            boot_and_run(&kernel, cmdline, vec![]).expect("second boot failed");

        assert_eq!(first_console, second_console, "console output must be identical across two boots");
        assert_eq!(first_hash, second_hash, "RAM hash must be identical across two boots");
    }

    /// Server-level analogue of `baud-multiverse`'s own
    /// `thousand_branches_are_independent_and_deterministic`: this route's own
    /// `boot_snapshot_and_branch` (the exact function the HTTP handler calls, minus only the
    /// axum/JSON plumbing) must fork branches that don't perturb each other and that replay
    /// deterministically from the same branch point + suffix.
    #[test]
    fn run_kvm_branch_produces_independent_and_deterministic_branches() {
        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let suffixes: Vec<Vec<u8>> = (0..6u8).map(|i| vec![i, 0xAA, 0xBB, 0xCC]).collect();

        let first_run = boot_snapshot_and_branch(&kernel, cmdline, suffixes.clone())
            .expect("boot_snapshot_and_branch failed");
        assert_eq!(first_run.len(), suffixes.len());
        for (i, (console_output, _ram_hash)) in first_run.iter().enumerate() {
            assert_eq!(
                console_output, &suffixes[i],
                "branch {i} must echo exactly its own tape suffix, not another branch's state"
            );
        }

        // Re-forking from a fresh branch point with the same suffixes must be byte-identical —
        // both across branches (no cross-branch bleed) and across this whole re-run (determinism).
        let second_run = boot_snapshot_and_branch(&kernel, cmdline, suffixes)
            .expect("second boot_snapshot_and_branch failed");
        assert_eq!(first_run, second_run, "re-forking the same suffixes must reproduce byte-identically");
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = vec![0x00, 0xAB, 0xFF, 0x10];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
        assert_eq!(hex_decode(""), Some(Vec::new()));
        assert_eq!(hex_decode("abc"), None, "odd-length hex must be rejected");
        assert_eq!(hex_decode("zz"), None, "non-hex characters must be rejected");
    }
}
