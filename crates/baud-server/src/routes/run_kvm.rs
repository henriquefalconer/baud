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

    #[test]
    fn hex_roundtrip() {
        let bytes = vec![0x00, 0xAB, 0xFF, 0x10];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
        assert_eq!(hex_decode(""), Some(Vec::new()));
        assert_eq!(hex_decode("abc"), None, "odd-length hex must be rejected");
        assert_eq!(hex_decode("zz"), None, "non-hex characters must be rejected");
    }
}
