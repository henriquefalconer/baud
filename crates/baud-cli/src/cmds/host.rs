// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud host — capability probe + regime decision (specs/baud-host.md, milestone H0)

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct HostCmd {
    #[command(subcommand)]
    pub action: HostAction,
}

#[derive(Subcommand)]
pub enum HostAction {
    /// Probe this host's KVM/VT-x capabilities and report the determinism regime it supports.
    Probe {
        /// Fail (exit 1) unless the probed regime is at least this strong, instead of silently
        /// reporting a weaker regime than the caller asked for (specs/baud-multiverse.md §3.8,
        /// todo.md test-matrix row 1's `regime_is_recorded_and_not_overclaimed`: "asking for
        /// enforced guarantees on such a host returns exit 1 with a clear message, not a false
        /// pass"). `cooperative` is satisfied by either regime; `enforced` only by
        /// `enforced-capable`.
        #[arg(long, value_enum)]
        require: Option<RequiredRegime>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum RequiredRegime {
    Cooperative,
    Enforced,
}

/// Whether a probed `regime` (the raw JSON string `baud-server` reports — `"cooperative"`,
/// `"enforced-capable"`, or `"rejected"`) meets a caller's `--require`d minimum. Never treats a
/// weaker regime as satisfying a stronger request — the whole point of recording the regime is to
/// refuse overclaiming it (todo.md test-matrix row 1).
fn regime_satisfies(regime: &str, required: RequiredRegime) -> bool {
    match required {
        RequiredRegime::Cooperative => regime == "cooperative" || regime == "enforced-capable",
        RequiredRegime::Enforced => regime == "enforced-capable",
    }
}

pub async fn run(cmd: HostCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        HostAction::Probe { require } => {
            let v = c.get("/host/probe").await?;
            fmt::print(&v, json);
            let regime = v.get("regime").and_then(|r| r.as_str()).unwrap_or("");
            // A required capability failed: this host cannot run baud at all (specs/baud-host.md
            // §4). Never a false pass — exit 1 so scripts/drives notice.
            if regime == "rejected" {
                std::process::exit(1);
            }
            if let Some(required) = require {
                if !regime_satisfies(regime, required) {
                    eprintln!(
                        "baud host probe: this host only supports regime '{regime}', which does \
                         not meet the requested '--require {required:?}' guarantee — refusing to \
                         report a stronger determinism guarantee than this host actually \
                         verified (exit 1, not a false pass)."
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

    /// specs/baud-multiverse.md §3.8 / §8, todo.md test-matrix row 1's
    /// `regime_is_recorded_and_not_overclaimed`: a run/probe must never report a stronger
    /// determinism guarantee than what was actually verified — asking for `enforced` on a
    /// `cooperative`-only host must fail, not silently pass.
    #[test]
    fn regime_is_recorded_and_not_overclaimed() {
        assert!(!regime_satisfies("cooperative", RequiredRegime::Enforced));
        assert!(regime_satisfies("enforced-capable", RequiredRegime::Cooperative));
        assert!(regime_satisfies("cooperative", RequiredRegime::Cooperative));
        assert!(regime_satisfies("enforced-capable", RequiredRegime::Enforced));
        assert!(!regime_satisfies("rejected", RequiredRegime::Cooperative));
        assert!(!regime_satisfies("rejected", RequiredRegime::Enforced));
    }
}
