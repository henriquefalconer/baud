// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use axum::{extract::State, Json};
use baud_keys;
use serde_json::{json, Value};
use crate::AppState;

/// GET /doctor — `baud doctor`
pub async fn doctor(_state: State<AppState>) -> Json<Value> {
    let report = baud_keys::doctor();

    Json(json!({
        "sops": {
            "ok": report.sops_ok,
            "version": report.sops_version,
        },
        "age": {
            "ok": report.age_ok,
            "version": report.age_version,
        },
        "age_key_path": report.age_key_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        "secrets_file_exists": report.secrets_file_exists,
        // Stubs for items checked at later milestones
        "daytona_reachable": null,
        "cross_toolchain_ok": null,
        "local_backend_vm_ok": null,
    }))
}
