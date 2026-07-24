// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use axum::{extract::State, Json};
use serde_json::{json, Value};
use crate::AppState;

/// GET /runs — `baud run ls`
pub async fn list(_state: State<AppState>) -> Json<Value> {
    // Stub: real implementation queries the SQLite runs table
    Json(json!({ "runs": [] }))
}
