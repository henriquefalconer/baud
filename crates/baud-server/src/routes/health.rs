// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use axum::{extract::State, Json};
use serde_json::{json, Value};
use crate::AppState;

pub async fn health(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "started_at": s.started_at,
    }))
}
