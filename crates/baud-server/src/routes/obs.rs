// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /obs observation routes.

use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    response::sse::{Event, Sse},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{convert::Infallible, time::Duration};

#[derive(Debug, Deserialize)]
pub struct ObsQuery {
    pub probe: Option<String>,
    pub node: Option<i64>,
}
type Row = (i64, i64, String, Vec<u8>, i64);

pub async fn list(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<ObsQuery>,
) -> Json<Value> {
    let rows = fetch_rows(&state, &run_id, q.probe.as_deref(), q.node, 0).await;
    Json(json!({ "run_id": run_id, "observations": rows_to_json(rows) }))
}

/// Live observation tail. It emits new rows and heartbeats while the run is quiet.
pub async fn tail(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<ObsQuery>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let probe = q.probe;
    let node = q.node;
    let stream = futures_util::stream::unfold(
        (state, run_id, probe, node, 0_i64),
        |(state, run_id, probe, node, last)| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let rows = fetch_rows(&state, &run_id, probe.as_deref(), node, last).await;
            let next = rows
                .iter()
                .map(|(step, _, _, _, _)| *step)
                .max()
                .unwrap_or(last);
            if rows.is_empty() {
                Some((
                    Ok(Event::default().event("heartbeat").data("{}")),
                    (state, run_id, probe, node, last),
                ))
            } else {
                let event = Event::default()
                    .event("observation")
                    .json_data(rows_to_json(rows))
                    .unwrap_or_else(|_| Event::default().data("{}"));
                Some((Ok(event), (state, run_id, probe, node, next)))
            }
        },
    );
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

pub async fn append(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<AppendObsBody>,
) -> Json<Value> {
    let result = sqlx::query("INSERT INTO observations (run_id, step, node, probe, value, recorded_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&run_id).bind(body.step as i64).bind(body.node as i64).bind(&body.probe)
        .bind(serde_json::to_vec(&body.value).unwrap_or_default()).bind(crate::state::unix_now() as i64)
        .execute(&state.db).await;
    match result {
        Ok(_) => Json(json!({ "ok": true, "run_id": run_id, "step": body.step })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

#[derive(Debug, Deserialize)]
pub struct AppendObsBody {
    pub step: u64,
    pub node: u16,
    pub probe: String,
    pub value: Value,
}

async fn fetch_rows(
    state: &AppState,
    run_id: &str,
    probe: Option<&str>,
    node: Option<i64>,
    after: i64,
) -> Vec<Row> {
    sqlx::query_as::<_, Row>("SELECT step, node, probe, value, recorded_at FROM observations WHERE run_id = ? AND (? IS NULL OR probe = ?) AND (? IS NULL OR node = ?) AND step > ? ORDER BY step ASC")
        .bind(run_id).bind(probe).bind(probe).bind(node).bind(node).bind(after).fetch_all(&state.db).await.unwrap_or_default()
}

fn rows_to_json(rows: Vec<Row>) -> Vec<Value> {
    rows.into_iter().map(|(step, node, probe, value, recorded_at)| json!({ "step": step, "node": node, "probe": probe, "value": decode_value_for_display(&value), "recorded_at": recorded_at })).collect()
}

fn decode_value_for_display(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(bytes.iter().map(|b| format!("{b:02x}")).collect()))
}
