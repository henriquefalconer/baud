// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Budget accounting routes (M9)
//
// GET  /budget              — total sandbox-minutes used this session + per-run breakdown
// POST /budget/record       — record sandbox minutes for a run

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;
use crate::state::unix_now;

/// GET /budget
pub async fn budget(State(s): State<AppState>) -> Json<Value> {
    let session_total = *s.budget_minutes_used.lock().await;

    // Per-run breakdown from DB (untyped to avoid DATABASE_URL requirement)
    let rows: Vec<(String, f64)> = sqlx::query_as(
        "SELECT run_id, SUM(sandbox_minutes) as total_minutes FROM run_budget GROUP BY run_id ORDER BY total_minutes DESC LIMIT 20"
    )
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();

    let db_total: f64 = rows.iter().map(|(_, m)| *m).sum();

    let per_run: Vec<Value> = rows
        .iter()
        .map(|(run_id, minutes)| json!({
            "run_id": run_id,
            "sandbox_minutes": minutes
        }))
        .collect();

    Json(json!({
        "sandbox_minutes_used": session_total,
        "db_total_minutes": db_total,
        "per_run": per_run,
        "recorded_at": unix_now(),
    }))
}

/// POST /budget/record — record sandbox-minutes for a run
#[derive(Debug, Deserialize)]
pub struct BudgetRecordBody {
    pub run_id: String,
    pub sandbox_minutes: f64,
}

pub async fn record(
    State(s): State<AppState>,
    Json(body): Json<BudgetRecordBody>,
) -> Json<Value> {
    // Update in-memory total
    {
        let mut used = s.budget_minutes_used.lock().await;
        *used = used.saturating_add(body.sandbox_minutes as u64);
    }

    // Persist per-run
    let res = sqlx::query(
        "INSERT INTO run_budget (run_id, sandbox_minutes) VALUES (?, ?)"
    )
    .bind(&body.run_id)
    .bind(body.sandbox_minutes)
    .execute(&s.db)
    .await;

    match res {
        Ok(_) => Json(json!({ "ok": true, "run_id": body.run_id, "sandbox_minutes": body.sandbox_minutes })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}
