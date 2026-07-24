// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /image — guest-image contract checks (todo.md §4, specs/baud-packages.md §9)
//
// Routes:
//   POST /image/lint → lint a guest kernel .config against the tape-device image contract
//                       (`baud image lint`; todo.md §4's `image_lint_requires_tape_driver`)

use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

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
