// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use axum::{extract::State, Json};
use serde_json::{json, Value};
use crate::AppState;

/// GET /budget — `baud budget`
pub async fn budget(State(s): State<AppState>) -> Json<Value> {
    let used = *s.budget_minutes_used.lock().await;
    Json(json!({
        "sandbox_minutes_used": used,
    }))
}
