// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use crate::AppState;
use axum::{extract::State, Json};
use baud_keys;
use serde_json::{json, Value};

/// Check whether the local backend VM (lima/colima) is available.
///
/// On macOS the supervisor requires a Linux kernel — LocalBackend runs inside a
/// lima VM. This function checks: (a) limactl is on PATH, (b) at least one VM
/// instance is in "Running" state. Returns true/false rather than null.
fn command_succeeds(command: &str, args: &[&str]) -> bool {
    std::process::Command::new(command)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn check_local_backend_vm() -> bool {
    // Check if limactl is installed
    let limactl = std::process::Command::new("which")
        .arg("limactl")
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !limactl {
        return false;
    }

    // Check if a lima VM is running
    let vm_running = std::process::Command::new("limactl")
        .args(["list", "--format", "{{.Status}}"])
        .output()
        .ok()
        .map(|o| {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.lines().any(|l| l.trim() == "Running")
        })
        .unwrap_or(false);

    vm_running
}

/// GET /doctor — `baud doctor`
pub async fn doctor(_state: State<AppState>) -> Json<Value> {
    let report = baud_keys::doctor();
    let host = tokio::task::spawn_blocking(baud_host::Host::probe)
        .await
        .expect("host doctor task panicked");

    Json(json!({
        "sops": {
            "ok": report.sops_ok,
            "version": report.sops_version,
        },
        "age": {
            "ok": report.age_ok,
            "version": report.age_version,
        },
        "ssh_to_age": {
            "ok": report.ssh_to_age_present,
            "version": report.ssh_to_age_version,
        },
        "age_key_path": report.age_key_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        "secrets_file_exists": report.secrets_file_exists,
        "is_recipient": report.is_recipient,
        // These checks are concrete booleans. A missing optional tool is a failed check,
        // never a null value that callers can accidentally treat as success.
        "daytona_reachable": command_succeeds("daytona", &["status"]),
        "cross_toolchain_ok": command_succeeds("cargo", &["--version"])
            && command_succeeds("gcc-13", &["--version"]),
        // local_backend_vm_ok: on macOS the supervisor needs a lima/colima VM (Linux kernel).
        // Check whether limactl is installed and a baud VM instance is available.
        "local_backend_vm_ok": check_local_backend_vm(),
        "host": {
            "regime": host.regime(),
            "runnable": host.is_runnable(),
            "enforced_capable": host.is_enforced_capable(),
            "reason": host.reason,
            "capacity": host.capacity(),
            "housekeeping_reserved": host.topology().housekeeping_reserved,
            "cores": host.topology().cores,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::command_succeeds;

    #[test]
    fn command_check_fails_closed_for_missing_program() {
        assert!(!command_succeeds("baud-command-that-does-not-exist", &[]));
    }

    #[test]
    fn command_check_reports_a_real_program() {
        assert!(command_succeeds("true", &[]));
        assert!(!command_succeeds("false", &[]));
    }
}
