// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::AppState;

#[derive(Deserialize)]
pub struct KeysInitBody {
    pub age_recipient: String,
}

/// POST /keys/init — `baud keys init`
pub async fn init(
    _state: State<AppState>,
    Json(_body): Json<KeysInitBody>,
) -> Json<Value> {
    // Real implementation would call baud_keys::init_secrets(...).
    // For the M0 skeleton, return a stub.
    Json(json!({
        "ok": true,
        "note": "keys init not yet fully implemented",
    }))
}

#[derive(Serialize)]
struct KeysShowResponse {
    daytona_api_key: &'static str,
    identity_root_key: &'static str,
}

/// GET /keys/show — `baud keys show --redacted`
pub async fn show(_state: State<AppState>) -> Json<Value> {
    Json(json!({
        "daytona_api_key": "[REDACTED]",
        "identity_root_key": "[REDACTED]",
    }))
}
