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

use axum::{http::StatusCode, Json};
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
