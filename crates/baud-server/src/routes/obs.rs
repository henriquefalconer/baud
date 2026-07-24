// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /obs — observation routes (M3: full SQLite-backed implementation)

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ObsQuery {
    pub probe: Option<String>,
    pub node: Option<i64>,
}

/// GET /runs/:id/obs — list observations for a run (from SQLite)
pub async fn list(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<ObsQuery>,
) -> Json<Value> {
    // Build dynamic query
    let rows = if let (Some(probe), Some(node)) = (&q.probe, q.node) {
        sqlx::query_as::<_, (i64, i64, String, Vec<u8>, i64)>(
            "SELECT step, node, probe, value, recorded_at FROM observations
             WHERE run_id = ? AND probe = ? AND node = ?
             ORDER BY step ASC"
        )
        .bind(&run_id)
        .bind(probe)
        .bind(node)
        .fetch_all(&state.db)
        .await
    } else if let Some(probe) = &q.probe {
        sqlx::query_as::<_, (i64, i64, String, Vec<u8>, i64)>(
            "SELECT step, node, probe, value, recorded_at FROM observations
             WHERE run_id = ? AND probe = ?
             ORDER BY step ASC"
        )
        .bind(&run_id)
        .bind(probe)
        .fetch_all(&state.db)
        .await
    } else if let Some(node) = q.node {
        sqlx::query_as::<_, (i64, i64, String, Vec<u8>, i64)>(
            "SELECT step, node, probe, value, recorded_at FROM observations
             WHERE run_id = ? AND node = ?
             ORDER BY step ASC"
        )
        .bind(&run_id)
        .bind(node)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query_as::<_, (i64, i64, String, Vec<u8>, i64)>(
            "SELECT step, node, probe, value, recorded_at FROM observations
             WHERE run_id = ?
             ORDER BY step ASC"
        )
        .bind(&run_id)
        .fetch_all(&state.db)
        .await
    };

    match rows {
        Ok(rows) => {
            let observations: Vec<Value> = rows.into_iter().map(|(step, node, probe, value, recorded_at)| {
                // value is stored as CBOR-encoded baud_proto::Value
                let val_str = decode_value_for_display(&value);
                json!({
                    "step": step,
                    "node": node,
                    "probe": probe,
                    "value": val_str,
                    "recorded_at": recorded_at,
                })
            }).collect();
            Json(json!({ "run_id": run_id, "observations": observations }))
        }
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

/// GET /runs/:id/obs/tail — list observations (M3: full list; SSE upgrade M5+)
pub async fn tail(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Json<Value> {
    let rows = sqlx::query_as::<_, (i64, i64, String, Vec<u8>, i64)>(
        "SELECT step, node, probe, value, recorded_at FROM observations
         WHERE run_id = ?
         ORDER BY step ASC"
    )
    .bind(&run_id)
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let observations: Vec<Value> = rows.into_iter().map(|(step, node, probe, value, recorded_at)| {
                let val_str = decode_value_for_display(&value);
                json!({
                    "step": step,
                    "node": node,
                    "probe": probe,
                    "value": val_str,
                    "recorded_at": recorded_at,
                })
            }).collect();
            Json(json!({ "run_id": run_id, "observations": observations }))
        }
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

/// POST /runs/:id/obs — append an observation (used by provisioning/agent)
pub async fn append(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<AppendObsBody>,
) -> Json<Value> {
    let now = crate::state::unix_now() as i64;

    // Encode value as CBOR bytes
    let value_bytes = encode_value(&body.value);

    let result = sqlx::query(
        "INSERT INTO observations (run_id, step, node, probe, value, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?)"
    )
    .bind(&run_id)
    .bind(body.step as i64)
    .bind(body.node as i64)
    .bind(&body.probe)
    .bind(&value_bytes)
    .bind(now)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(json!({ "ok": true, "run_id": run_id, "step": body.step })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct AppendObsBody {
    pub step: u64,
    pub node: u16,
    pub probe: String,
    pub value: serde_json::Value,
}

fn encode_value(v: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

fn decode_value_for_display(bytes: &[u8]) -> serde_json::Value {
    // Try JSON first (our encoding), fall back to displaying as hex
    if let Ok(v) = serde_json::from_slice(bytes) {
        return v;
    }
    // Fall back: display as hex string
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    serde_json::Value::String(hex)
}
