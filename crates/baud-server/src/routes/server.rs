// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use axum::{extract::State, Json};
use serde_json::{json, Value};
use crate::AppState;

/// GET /server/status — `baud server status`
pub async fn status(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "running",
        "started_at": s.started_at,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// GET /server/logs — `baud server logs`
/// In the skeleton this returns an empty list; a real implementation streams the log file.
pub async fn logs(_state: State<AppState>) -> Json<Value> {
    Json(json!({
        "logs": [],
        "note": "streaming logs not yet implemented"
    }))
}
