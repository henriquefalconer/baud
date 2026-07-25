// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud image — guest-image contract checks (todo.md §4, specs/baud-packages.md §9)

use anyhow::Result;
use base64::Engine;
use clap::{Parser, Subcommand};
use serde_json::json;
use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct ImageCmd {
    #[command(subcommand)]
    pub action: ImageAction,
}

#[derive(Subcommand)]
pub enum ImageAction {
    /// Lint a guest kernel .config against the tape-device image contract: the tape-device driver
    /// must be enabled and no real hardware timer (RTC/HPET) baud does not model may be enabled.
    Lint {
        /// Path to the guest kernel's `.config` file.
        path: String,
    },
    /// Apply the build-time `rdseed`→`UD2`(+`NOP`) rewrite pass (todo.md §3.8/§4) to an ELF: the
    /// current dev host cannot hardware-trap `rdseed` (§3.8's host-capability note), so every
    /// `rdseed` opcode in the image's executable sections is rewritten in place before boot.
    RewriteRdseed {
        /// Path to the guest image ELF (kernel or an in-guest agent/userspace binary) to rewrite.
        path: String,
        /// Where to write the patched ELF. Defaults to overwriting `path` in place.
        #[arg(short, long)]
        output: Option<String>,
    },
}

pub async fn run(cmd: ImageCmd, c: &Client, json: bool) -> Result<()> {
    match cmd.action {
        ImageAction::Lint { path } => {
            let content = std::fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("failed to read kernel config '{}': {}", path, e)
            })?;
            let body = json!({ "content": content });
            let v = c.post("/image/lint", &body).await?;
            let ok = v.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            fmt::print(&v, json);
            // Never a false pass: a contract violation exits 1 so scripts/drives notice
            // (mirrors `baud host probe`'s not-runnable handling and `baud spec lint`'s not-ok
            // handling).
            if !ok {
                std::process::exit(1);
            }
        }
        ImageAction::RewriteRdseed { path, output } => {
            let elf_bytes = std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("failed to read guest image '{}': {}", path, e))?;
            let content_base64 = base64::engine::general_purpose::STANDARD.encode(&elf_bytes);
            let body = json!({ "content_base64": content_base64 });
            let v = c.post("/image/rewrite-rdseed", &body).await?;
            let patched_base64 = v
                .get("patched_base64")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("server response missing patched_base64: {v}"))?;
            let patched = base64::engine::general_purpose::STANDARD
                .decode(patched_base64)
                .map_err(|e| anyhow::anyhow!("server returned invalid base64: {e}"))?;
            let out_path = output.unwrap_or_else(|| path.clone());
            std::fs::write(&out_path, &patched)
                .map_err(|e| anyhow::anyhow!("failed to write patched image '{}': {}", out_path, e))?;

            // Persist the rewrite-site table as a sidecar next to the patched image (todo.md §14's
            // "RdseedRewriteReport -> boot wiring" gap): `baud-server`'s boot routes look up
            // `<kernel_path>.rdseed-sites.json` and, if present, thread its sites into
            // `Multiverse::boot_with_rdseed_sites` so a real `rdseed`-rewritten guest can actually
            // have its `UD2` sites served a value, not just the hand-built test fixtures. Always
            // written (even with zero sites) so a later boot never falls back to a stale sidecar
            // from a previous build of the same path.
            let sites = v.get("sites").cloned().unwrap_or_else(|| json!([]));
            let sidecar_path = format!("{out_path}.rdseed-sites.json");
            let sidecar = json!({ "sites": sites });
            std::fs::write(&sidecar_path, serde_json::to_vec_pretty(&sidecar)?).map_err(|e| {
                anyhow::anyhow!("failed to write rdseed-sites sidecar '{}': {}", sidecar_path, e)
            })?;

            // Echo the server's report (sites rewritten, count) rather than the (now redundant)
            // patched-image payload — a human/script wants to know what changed, not re-see the
            // bytes it just told the CLI to write to disk.
            let mut report = v.clone();
            if let Some(obj) = report.as_object_mut() {
                obj.remove("patched_base64");
                obj.insert("output_path".to_string(), json!(out_path));
                obj.insert("rdseed_sites_path".to_string(), json!(sidecar_path));
            }
            fmt::print(&report, json);
        }
    }
    Ok(())
}
