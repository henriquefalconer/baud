// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /image — guest-image contract checks (todo.md §4, specs/baud-packages.md §9)
//
// Routes:
//   POST /image/lint            → lint a guest kernel .config against the tape-device image
//                                  contract (`baud image lint`; todo.md §4's
//                                  `image_lint_requires_tape_driver`)
//   POST /image/rewrite-rdseed  → apply the build-time `rdseed`→`UD2`(+`NOP`) rewrite pass to a
//                                  guest image ELF (`baud image rewrite-rdseed`; todo.md §3.8/§4,
//                                  `image_rewrites_rdseed` / `no_rdseed_opcode_survives_in_image`)
//   POST /image/build           → build a real reproducible bzImage + initramfs.cpio.gz pair
//                                  (`baud image build`; todo.md §4.5/§14 next-actions item 1:
//                                  compose `baud_packages::build_bzimage` +
//                                  `build_reproducible_initramfs` end-to-end). Every path in the
//                                  request body is resolved on this server host, not transferred
//                                  content -- a kernel source tree is far too large to shuttle
//                                  over HTTP, unlike `/image/rewrite-rdseed`'s single small ELF.

use axum::{http::StatusCode, Json};
use std::path::PathBuf;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg.into() })))
}

#[derive(Debug, Deserialize)]
pub struct ImageLintBody {
    /// Raw Linux kernel `.config` text.
    pub content: String,
}

/// POST /image/lint — lint a guest kernel .config
pub async fn lint(Json(body): Json<ImageLintBody>) -> Json<Value> {
    let report = baud_packages::lint_kernel_config(&body.content);
    Json(json!({
        "ok": report.ok(),
        "violations": report.violations,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RewriteRdseedBody {
    /// The guest image (or any single ELF within it, e.g. the kernel or an in-guest agent
    /// binary), base64-encoded — this is a binary rewrite, unlike `/image/lint`'s plain-text
    /// `.config`.
    pub content_base64: String,
}

/// POST /image/rewrite-rdseed — apply the build-time `rdseed`→`UD2`(+`NOP`) rewrite pass
/// (todo.md §4) to an ELF and hand back the patched bytes plus every site touched.
pub async fn rewrite_rdseed(Json(body): Json<RewriteRdseedBody>) -> ApiResult {
    let elf_bytes = base64::engine::general_purpose::STANDARD
        .decode(&body.content_base64)
        .map_err(|e| bad_request(format!("content_base64 is not valid base64: {e}")))?;

    let (patched, report) = baud_packages::rewrite_rdseed(&elf_bytes)
        .map_err(|e| bad_request(format!("rdseed rewrite failed: {e:#}")))?;

    let patched_base64 = base64::engine::general_purpose::STANDARD.encode(&patched);
    Ok(Json(json!({
        "ok": true,
        "count": report.count(),
        "sites": report.sites,
        "patched_base64": patched_base64,
    })))
}

#[derive(Debug, Deserialize)]
pub struct ImageBuildInitramfsEntry {
    /// Path inside the initramfs archive, e.g. `"init"`.
    pub archive_path: String,
    /// Unix permission bits, e.g. `0o755` (493).
    pub mode: u32,
    /// Path to the file's contents on this server host.
    pub source_path: String,
}

fn default_cc() -> String {
    "gcc-13".to_string()
}

#[derive(Debug, Deserialize)]
pub struct ImageBuildBody {
    /// A **writable, disposable** kernel source tree on this server host (see
    /// `KernelBuildConfig::kernel_src`'s own doc — never the shared `~/wsl-kernel-src/src` tree).
    pub kernel_src: String,
    /// A Kconfig fragment merged onto `allnoconfig` (spec §4.1's required/disabled list).
    pub config_fragment: String,
    #[serde(default = "default_cc")]
    pub cc: String,
    /// Parallel build jobs (`make -jN`). Omitted/null uses the host's available parallelism.
    pub jobs: Option<usize>,
    pub initramfs_entries: Vec<ImageBuildInitramfsEntry>,
    /// Directory the built `bzImage` and `initramfs.cpio.gz` are written into on this server host.
    pub output_dir: String,
}

/// POST /image/build — build a real, reproducible `bzImage` + `initramfs.cpio.gz` pair and
/// report spec §4.5's image identity hash. Real kernel builds take minutes and shell out to
/// `make`, so this runs in `spawn_blocking` like every other real-hardware/real-process route
/// (`/host/probe`, `/run/kvm`) rather than blocking the async runtime.
pub async fn build(Json(body): Json<ImageBuildBody>) -> ApiResult {
    let result = tokio::task::spawn_blocking(move || {
        let kernel_src = PathBuf::from(&body.kernel_src);
        let config_fragment = PathBuf::from(&body.config_fragment);
        let output_dir = PathBuf::from(&body.output_dir);
        let entries: Vec<baud_packages::InitramfsFileEntry> = body
            .initramfs_entries
            .iter()
            .map(|e| baud_packages::InitramfsFileEntry {
                archive_path: e.archive_path.clone(),
                mode: e.mode,
                source_path: PathBuf::from(&e.source_path),
            })
            .collect();
        let cfg = baud_packages::GuestImageBuildConfig {
            kernel: baud_packages::KernelBuildConfig {
                kernel_src: &kernel_src,
                config_fragment: &config_fragment,
                cc: &body.cc,
                jobs: body.jobs,
            },
            initramfs_entries: &entries,
            output_dir: &output_dir,
        };
        baud_packages::build_guest_image(&cfg)
    })
    .await
    .expect("image/build task panicked");

    match result {
        Ok(r) => Ok(Json(json!({
            "ok": true,
            "bzimage_path": r.bzimage_path.display().to_string(),
            "initramfs_path": r.initramfs_path.display().to_string(),
            "bzimage_sha256": r.bzimage_sha256,
            "initramfs_sha256": r.initramfs_sha256,
            "image_hash": r.image_hash,
        }))),
        Err(e) => Err(bad_request(format!("guest image build failed: {e:#}"))),
    }
}
