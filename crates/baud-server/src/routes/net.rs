// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /net — M5 network weather timeline routes
//
// Routes:
//   GET  /runs/:id/net/weather          → weather timeline (partition/delay events)
//   POST /runs/:id/net/weather          → append a weather event (from agent)

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;
use crate::state::unix_now;

// ---------------------------------------------------------------------------
// POST /runs/:id/net/weather — record a net event
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NetEventBody {
    pub step: u64,
    pub kind: String,
    pub from_node: Option<u16>,
    pub to_node: Option<u16>,
    pub delay_ticks: Option<u64>,
    pub drop_prob: Option<f64>,
}

pub async fn append_event(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<NetEventBody>,
) -> Json<Value> {
    let now = unix_now() as i64;

    let result = sqlx::query(
        "INSERT INTO net_events (run_id, step, kind, from_node, to_node, delay_ticks, drop_prob, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&run_id)
    .bind(body.step as i64)
    .bind(&body.kind)
    .bind(body.from_node.map(|v| v as i64))
    .bind(body.to_node.map(|v| v as i64))
    .bind(body.delay_ticks.map(|v| v as i64))
    .bind(body.drop_prob)
    .bind(now)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(json!({ "ok": true, "run_id": run_id, "step": body.step })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// GET /runs/:id/net/weather — return the recorded partition/delay timeline
// ---------------------------------------------------------------------------

pub async fn weather(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Json<Value> {
    let rows = sqlx::query_as::<_, (i64, String, Option<i64>, Option<i64>, Option<i64>, Option<f64>)>(
        "SELECT step, kind, from_node, to_node, delay_ticks, drop_prob
         FROM net_events
         WHERE run_id = ?
         ORDER BY step ASC"
    )
    .bind(&run_id)
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let events: Vec<Value> = rows.into_iter().map(|(step, kind, from_node, to_node, delay_ticks, drop_prob)| {
                json!({
                    "step": step,
                    "kind": kind,
                    "from_node": from_node,
                    "to_node": to_node,
                    "delay_ticks": delay_ticks,
                    "drop_prob": drop_prob,
                })
            }).collect();
            Json(json!({ "run_id": run_id, "weather": events }))
        }
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// Simulation helper — generate a synthetic weather timeline for a 3-node run
// (used by drive/m/m5.sh to seed the weather table for testing)
// ---------------------------------------------------------------------------

pub async fn simulate_weather(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Json<Value> {
    let now = unix_now() as i64;
    // Generate a Markov partition sequence: on at step 10, off at step 30
    let events = vec![
        (10i64, "partition_on", None::<i64>, None::<i64>, None::<i64>, None::<f64>),
        (30i64, "partition_off", None, None, None, None),
        (50i64, "delay", Some(0), Some(1), Some(5i64), None),
        (70i64, "delay", Some(1), Some(2), Some(3i64), None),
        (90i64, "partition_on", None, None, None, None),
        (110i64, "partition_off", None, None, None, None),
    ];

    // Clear existing events for this run
    let _ = sqlx::query("DELETE FROM net_events WHERE run_id = ?")
        .bind(&run_id)
        .execute(&state.db)
        .await;

    let mut count = 0usize;
    for (step, kind, from_node, to_node, delay_ticks, drop_prob) in events {
        let r = sqlx::query(
            "INSERT INTO net_events (run_id, step, kind, from_node, to_node, delay_ticks, drop_prob, recorded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&run_id)
        .bind(step)
        .bind(kind)
        .bind(from_node)
        .bind(to_node)
        .bind(delay_ticks)
        .bind(drop_prob)
        .bind(now)
        .execute(&state.db)
        .await;
        if r.is_ok() { count += 1; }
    }

    Json(json!({ "ok": true, "run_id": run_id, "events_generated": count }))
}
