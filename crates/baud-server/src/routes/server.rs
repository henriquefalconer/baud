// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use axum::{extract::{Query, State}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::AppState;

/// GET /server/status — `baud server status`
pub async fn status(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "running",
        "started_at": s.started_at,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Deserialize)]
pub struct LogsQuery {
    /// Return only log entries with seq > after (cursor-based pagination)
    #[serde(default)]
    after: u64,
}

/// GET /server/logs?after=N — `baud server logs [--follow]`
///
/// Returns server log lines from the in-process ring buffer.
/// The `after` query parameter acts as a cursor: only entries whose `seq`
/// is strictly greater than `after` are returned.  The CLI uses this for
/// long-polling / follow mode: it remembers the last `seq` and re-polls.
pub async fn logs(State(state): State<AppState>, Query(q): Query<LogsQuery>) -> Json<Value> {
    let entries = state.log_buffer.read().unwrap();
    let filtered: Vec<_> = entries
        .iter()
        .filter(|e| e.seq > q.after)
        .map(|e| json!({ "seq": e.seq, "ts": e.ts, "level": e.level, "msg": e.msg }))
        .collect();
    let last_seq = filtered.last().and_then(|e| e.get("seq").and_then(|v| v.as_u64())).unwrap_or(q.after);
    Json(json!({
        "logs": filtered,
        "last_seq": last_seq,
    }))
}

/// A single log entry held in the server's ring buffer.
#[derive(Clone)]
pub struct LogEntry {
    pub seq: u64,
    pub ts: u64,
    pub level: String,
    pub msg: String,
}

/// Push a log entry into the ring buffer (called by the tracing subscriber).
#[allow(dead_code)]
pub fn push_log(state: &AppState, level: &str, msg: &str) {
    let mut buf = state.log_buffer.write().unwrap();
    let seq = buf.last().map(|e| e.seq + 1).unwrap_or(1);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    buf.push(LogEntry { seq, ts, level: level.to_string(), msg: msg.to_string() });
    // Keep at most 4096 entries (ring buffer behaviour)
    let len = buf.len();
    if len > 4096 {
        buf.drain(..len - 4096);
    }
}
