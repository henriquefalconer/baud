// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// POST /verify/fingerprint — H9's cross-VM determinism check (specs/baud-fingerprint.md,
// specs/baud-ubuntu.md §6): boot the same (kernel, cmdline, tape) `times` times (default 2, like
// `/verify/determinism`), each independently, capture a timed-exit `Fingerprint` at `target_rcb`
// from each boot (`baud_fingerprint::capture`, todo.md §14 item 9), and compare every later
// fingerprint against the first (`baud_fingerprint::compare`) — the first divergence, if any, is
// reported by field name, never masked by comparing only rendered text.
//
// This closes the "`baud verify fingerprint` CLI (needs a new `baud-server` HTTP route, since
// `baud-cli` is HTTP-only)" gap todo.md §14 item 9 named as still open. It still boots each VM
// sequentially in this one server process — the same same-process stand-in
// `baud_fingerprint::linux::tests::two_independent_boots_produce_matching_fingerprints` uses for
// H9's true two-separate-process/two-core orchestration (`baud_multiverse::linux::run_fleet` is
// the closest existing per-process-style primitive, still per-thread not per-process, and still
// not wired to any route) — that piece, and the real Ubuntu 18.04.1 cloud image, remain open.
// Linux-only, like every route calling into `baud_multiverse::linux`/`baud_fingerprint::capture`.

use axum::Json;
use baud_fingerprint::Fingerprint;
use baud_multiverse::linux::Multiverse;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct VerifyFingerprintBody {
    /// Path to a bzImage kernel on this host's filesystem — same convention as `RunKvmBody::
    /// kernel_path` (`routes::run_kvm`).
    pub kernel_path: String,
    /// Kernel command line. Defaults to spec §4.2's deterministic cmdline, same as `/run/kvm`.
    #[serde(default = "default_cmdline")]
    pub cmdline: String,
    /// The run's whole tape, hex-encoded (empty tape if omitted).
    #[serde(default)]
    pub tape_hex: String,
    /// Deterministic-event count (retired conditional branches) at which to stop and capture the
    /// fingerprint (specs/baud-fingerprint.md §4).
    pub target_rcb: u64,
    /// Number of trailing console bytes to slice as the banner (§5). Defaults to 64, matching the
    /// crate's own tests.
    #[serde(default = "default_banner_tail_len")]
    pub banner_tail_len: usize,
    /// Hex-encoded expected banner bytes. When set, a captured tail not ending with this fails the
    /// whole call with `FpError::NoBanner` rather than comparing a fingerprint for the wrong point.
    /// Omit for a guest that prints no recognizable banner (every fixture in this workspace today).
    #[serde(default)]
    pub expected_banner_hex: Option<String>,
    /// Number of independent boots to compare (minimum 2, default 2 — same convention as
    /// `/verify/determinism`'s `times`).
    #[serde(default = "default_times")]
    pub times: u32,
    /// Path to a reproducible initramfs on this host's filesystem, same as `RunKvmBody::
    /// initramfs_path`. `None` for a guest with no separate initramfs.
    #[serde(default)]
    pub initramfs_path: Option<String>,
}

fn default_cmdline() -> String {
    baud_multiverse::linux::bootparams::DETERMINISTIC_CMDLINE.to_owned()
}

fn default_banner_tail_len() -> usize {
    64
}

fn default_times() -> u32 {
    2
}

pub async fn fingerprint(Json(body): Json<VerifyFingerprintBody>) -> Json<Value> {
    let tape = match hex_decode(&body.tape_hex) {
        Some(t) => t,
        None => return Json(json!({ "ok": false, "error": "tape_hex must be a valid hex string" })),
    };
    let expected_banner = match &body.expected_banner_hex {
        Some(hex) => match hex_decode(hex) {
            Some(bytes) => Some(bytes),
            None => {
                return Json(json!({ "ok": false, "error": "expected_banner_hex must be a valid hex string" }))
            }
        },
        None => None,
    };
    let initramfs = match &body.initramfs_path {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                return Json(json!({
                    "ok": false,
                    "error": format!("failed to read initramfs_path '{path}': {e}"),
                }))
            }
        },
        None => None,
    };

    let times = body.times.max(2);
    let kernel_path = PathBuf::from(&body.kernel_path);
    let cmdline = body.cmdline.clone();
    let target_rcb = body.target_rcb;
    let banner_tail_len = body.banner_tail_len;

    // Real ioctls (KVM_RUN and friends) block; keep them off the async executor, same convention
    // as `/run/kvm`.
    let result = tokio::task::spawn_blocking(move || {
        boot_and_compare_fingerprints(
            &kernel_path,
            &cmdline,
            tape,
            initramfs.as_deref(),
            target_rcb,
            banner_tail_len,
            expected_banner.as_deref(),
            times,
        )
    })
    .await
    .expect("verify/fingerprint task panicked");

    match result {
        Ok((fingerprints, divergence)) => {
            let verified = divergence.is_none();
            Json(json!({
                "ok": verified,
                "verified": verified,
                "times": times,
                "target_rcb": target_rcb,
                "fingerprints": fingerprints.iter().map(render_fingerprint_json).collect::<Vec<_>>(),
                "divergence": divergence.map(|d| json!({
                    "field": d.field,
                    "a": d.a,
                    "b": d.b,
                })),
                "message": if verified {
                    format!("determinism verified: all {times} boots produced matching fingerprints at event {target_rcb}")
                } else {
                    "DETERMINISM VIOLATION: boots produced diverging fingerprints".to_string()
                },
            }))
        }
        Err(e) => Json(json!({ "ok": false, "error": e })),
    }
}

fn render_fingerprint_json(f: &Fingerprint) -> Value {
    json!({
        "label": f.label,
        "banner_hex": hex_encode(&f.banner),
        "events": f.events,
        "rip": format!("{:#018x}", f.rip),
        "gpa": f.gpa.map(|g| format!("{g:#018x}")),
        "mem_hash": f.mem_hash,
        "report": f.render(),
    })
}

/// Boot `kernel_path` `times` times, sequentially, each independently, capturing a `Fingerprint`
/// at `target_rcb` from each boot; compares every fingerprint after the first against the first
/// one (`baud_fingerprint::compare`), stopping at the first divergence. Returns every captured
/// fingerprint alongside the first divergence found, if any — a caller wants the fingerprints
/// either way (for a report), not just a bool.
#[allow(clippy::too_many_arguments)]
fn boot_and_compare_fingerprints(
    kernel_path: &Path,
    cmdline: &str,
    tape: Vec<u8>,
    initramfs: Option<&[u8]>,
    target_rcb: u64,
    banner_tail_len: usize,
    expected_banner: Option<&[u8]>,
    times: u32,
) -> Result<(Vec<Fingerprint>, Option<baud_fingerprint::Divergence>), String> {
    let rdseed_sites = crate::rdseed_sites::load_rdseed_sites(kernel_path)?;
    let mut fingerprints = Vec::with_capacity(times as usize);
    for i in 0..times {
        let mut vm = Multiverse::boot_with_rdseed_sites(
            kernel_path,
            cmdline,
            0,
            1,
            tape.clone(),
            None,
            initramfs,
            rdseed_sites.iter().map(|(addr, site)| (*addr, *site)),
        )
        .map_err(|e| format!("vm{i} boot failed: {e}"))?;
        let f = baud_fingerprint::capture(&mut vm, &format!("vm{i}"), target_rcb, banner_tail_len, expected_banner)
            .map_err(|e| format!("vm{i} capture failed: {e}"))?;
        fingerprints.push(f);
    }

    let mut divergence = None;
    for f in &fingerprints[1..] {
        if let Err(d) = baud_fingerprint::compare(&fingerprints[0], f) {
            divergence = Some(d);
            break;
        }
    }
    Ok((fingerprints, divergence))
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

    fn timer_guest_kernel_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/timer-guest/bzImage")
    }

    /// Server-route-level analogue of `baud_fingerprint`'s own
    /// `two_independent_boots_produce_matching_fingerprints`: proves the route's boot-N-times-and-
    /// compare wrapper (including its `rdseed_sites` sidecar lookup, `/run/kvm`'s own convention)
    /// carries the same whole-machine determinism property through to a real HTTP-shaped response.
    #[test]
    fn boot_and_compare_fingerprints_reports_no_divergence_across_two_boots() {
        let kernel = timer_guest_kernel_path();
        let (fingerprints, divergence) = boot_and_compare_fingerprints(
            &kernel,
            "console=ttyS0",
            vec![],
            None,
            100_000,
            64,
            None,
            2,
        )
        .expect("boot_and_compare_fingerprints failed");

        assert_eq!(fingerprints.len(), 2);
        assert_ne!(fingerprints[0].label, fingerprints[1].label);
        assert!(divergence.is_none(), "two independent boots must not diverge: {divergence:?}");
    }

    /// The route must refuse to report a fingerprint for the wrong point rather than silently
    /// comparing whatever it captured — same contract as `baud_fingerprint`'s own
    /// `wrong_expected_banner_is_rejected`, exercised through this route's wrapper.
    #[test]
    fn boot_and_compare_fingerprints_propagates_a_missing_banner_as_an_error() {
        let kernel = timer_guest_kernel_path();
        let err = boot_and_compare_fingerprints(
            &kernel,
            "console=ttyS0",
            vec![],
            None,
            100_000,
            64,
            Some(b"a banner timer-guest never prints"),
            2,
        )
        .expect_err("timer-guest's console never contains this banner");
        assert!(err.contains("capture failed"));
    }
}
