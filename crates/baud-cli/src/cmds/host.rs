// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud host — capability probe (specs/baud-host.md, milestone H0)

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct HostCmd {
    #[command(subcommand)]
    pub action: HostAction,
}

#[derive(Subcommand)]
pub enum HostAction {
    /// Probe this host's KVM/VT-x capabilities.
    Probe {
        /// Fail (exit 1) unless this host reports at least this capability, instead of silently
        /// proceeding on a host that doesn't actually support it (specs/baud-multiverse.md §3.8,
        /// todo.md test-matrix row 1's `capability_is_recorded_and_not_overclaimed`: "asking for
        /// enforced guarantees on such a host returns exit 1 with a clear message, not a false
        /// pass"). `cooperative` is satisfied by `runnable`; `enforced` only by
        /// `enforced_capable`.
        #[arg(long, value_enum)]
        require: Option<RequiredCapability>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum RequiredCapability {
    Cooperative,
    Enforced,
}

/// Whether a probed host (the raw JSON `baud-server` reports, carrying `runnable`/
/// `enforced_capable` booleans derived from `baud_host::Probe`) meets a caller's `--require`d
/// minimum. Never treats a weaker capability as satisfying a stronger request — the whole point
/// of recording it is to refuse overclaiming it (todo.md test-matrix row 1).
fn capability_satisfies(probe: &Value, required: RequiredCapability) -> bool {
    let runnable = probe.get("runnable").and_then(Value::as_bool).unwrap_or(false);
    let enforced_capable = probe.get("enforced_capable").and_then(Value::as_bool).unwrap_or(false);
    match required {
        RequiredCapability::Cooperative => runnable,
        RequiredCapability::Enforced => enforced_capable,
    }
}

pub async fn run(cmd: HostCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        HostAction::Probe { require } => {
            let v = c.get("/host/probe").await?;
            fmt::print(&v, json);
            let runnable = v.get("runnable").and_then(Value::as_bool).unwrap_or(false);
            // A required capability failed: this host cannot run baud at all (specs/baud-host.md
            // §4). Never a false pass — exit 1 so scripts/drives notice.
            if !runnable {
                std::process::exit(1);
            }
            if let Some(required) = require {
                if !capability_satisfies(&v, required) {
                    eprintln!(
                        "baud host probe: this host does not meet the requested \
                         '--require {required:?}' capability — refusing to report a stronger \
                         determinism guarantee than this host actually verified (exit 1, not a \
                         false pass)."
                    );
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// specs/baud-multiverse.md §3.8 / §8, todo.md test-matrix row 1's
    /// `capability_is_recorded_and_not_overclaimed`: a run/probe must never report a stronger
    /// determinism guarantee than what was actually verified — asking for `enforced` on a
    /// `cooperative`-only host must fail, not silently pass.
    #[test]
    fn capability_is_recorded_and_not_overclaimed() {
        let cooperative_only = json!({"runnable": true, "enforced_capable": false});
        let enforced = json!({"runnable": true, "enforced_capable": true});
        let rejected = json!({"runnable": false, "enforced_capable": false});

        assert!(!capability_satisfies(&cooperative_only, RequiredCapability::Enforced));
        assert!(capability_satisfies(&enforced, RequiredCapability::Cooperative));
        assert!(capability_satisfies(&cooperative_only, RequiredCapability::Cooperative));
        assert!(capability_satisfies(&enforced, RequiredCapability::Enforced));
        assert!(!capability_satisfies(&rejected, RequiredCapability::Cooperative));
        assert!(!capability_satisfies(&rejected, RequiredCapability::Enforced));
    }
}
