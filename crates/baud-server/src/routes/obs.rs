// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /obs — observation routes (M2 stub; full impl M3+)

use axum::{extract::{Path, Query, State}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ObsQuery {
    pub probe: Option<String>,
    pub node: Option<i64>,
}

/// GET /runs/:id/obs — list observations for a run
pub async fn list(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<ObsQuery>,
) -> Json<Value> {
    // Stub: return empty observations (full impl M3)
    let _ = (&state, &run_id, &q);
    Json(json!({ "observations": [], "run_id": run_id }))
}

/// GET /runs/:id/obs/tail — SSE stream of observations (stub)
pub async fn tail(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Json<Value> {
    let _ = (&state, &run_id);
    Json(json!({ "observations": [], "run_id": run_id, "note": "SSE tail not yet implemented (M3+)" }))
}
