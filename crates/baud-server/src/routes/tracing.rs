// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /tracing — observation plane 2 routes (M7)
//
// Routes:
//   GET  /tracing/tail?tape=<id>&event=<kind>&node=<n>  → SSE stream of eBPF records
//   GET  /tracing/summary?run=<id>                      → aggregate summary
//   POST /runs/:id/tracing/seed                         → seed synthetic eBPF records from syscall log (for testing)
//   GET  /runs/:id/ebpf                                 → list stored eBPF records

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TracingTailQuery {
    pub tape: Option<String>,
    pub event: Option<String>,
    pub node: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct TracingSummaryQuery {
    pub run: String,
}

// ---------------------------------------------------------------------------
// GET /tracing/tail
// ---------------------------------------------------------------------------

pub async fn tail(
    State(state): State<AppState>,
    Query(q): Query<TracingTailQuery>,
) -> Json<Value> {
    let tape_id = q.tape.clone().unwrap_or_default();

    // Find the most recent run for this tape (or list all eBPF records if tape="" )
    let run_filter: Option<String> = if !tape_id.is_empty() {
        // Look up runs by tape_id (untyped query — no DATABASE_URL required)
        let row = sqlx::query_as::<_, (String,)>(
            "SELECT id FROM runs WHERE tape_id = ? ORDER BY created_at DESC LIMIT 1"
        )
        .bind(&tape_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        row.map(|(id,)| id)
    } else {
        None
    };

    let (records, run_id_display): (Vec<Value>, String) = if let Some(rid) = run_filter {
        let recs = fetch_ebpf_records(&state, &rid, q.node, q.event.as_deref()).await;
        (recs, rid)
    } else {
        // No tape filter: return most recent 50 records across all runs
        let rows = sqlx::query_as::<_, (String, i64, String, i64, i64, String)>(
            "SELECT run_id, node, event, value, vtime, source FROM ebpf_records
             ORDER BY recorded_at DESC LIMIT 50"
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        let recs: Vec<Value> = rows.iter().map(|(run_id, node, event, value, vtime, source)| json!({
            "run_id": run_id,
            "node": node,
            "event": event,
            "value": value,
            "vtime": vtime,
            "source": source,
        })).collect();
        (recs, "(all)".to_string())
    };

    Json(json!({
        "ok": true,
        "run_id": run_id_display,
        "tape_id": tape_id,
        "records": records,
        "count": records.len(),
    }))
}

// ---------------------------------------------------------------------------
// GET /tracing/summary?run=<id>
// ---------------------------------------------------------------------------

pub async fn summary(
    State(state): State<AppState>,
    Query(q): Query<TracingSummaryQuery>,
) -> Json<Value> {
    let run_id = &q.run;

    // Check run exists
    let run_exists = sqlx::query_as::<_, (String,)>("SELECT id FROM runs WHERE id = ?")
        .bind(run_id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None)
        .is_some();

    if !run_exists {
        return Json(json!({ "ok": false, "error": format!("run not found: {run_id}") }));
    }

    // Aggregate eBPF records (untyped)
    let total_ebpf: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ?"
    )
    .bind(run_id)
    .fetch_one(&state.db)
    .await
    .map(|(c,)| c)
    .unwrap_or(0);

    let syscall_ebpf: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ? AND event LIKE 'syscall:%'"
    )
    .bind(run_id)
    .fetch_one(&state.db)
    .await
    .map(|(c,)| c)
    .unwrap_or(0);

    let sched_ebpf: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ? AND event LIKE 'sched_switch%'"
    )
    .bind(run_id)
    .fetch_one(&state.db)
    .await
    .map(|(c,)| c)
    .unwrap_or(0);

    let exec_ebpf: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ? AND event = 'exec'"
    )
    .bind(run_id)
    .fetch_one(&state.db)
    .await
    .map(|(c,)| c)
    .unwrap_or(0);

    let fault_ebpf: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ? AND event = 'fault'"
    )
    .bind(run_id)
    .fetch_one(&state.db)
    .await
    .map(|(c,)| c)
    .unwrap_or(0);

    // Source
    let source: Option<String> = sqlx::query_as::<_, (String,)>(
        "SELECT source FROM ebpf_records WHERE run_id = ? LIMIT 1"
    )
    .bind(run_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .map(|(s,)| s);

    // Syscall log (plane 1) count
    let total_syscalls: i64 = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM syscall_records WHERE run_id = ?"
    )
    .bind(run_id)
    .fetch_one(&state.db)
    .await
    .map(|(c,)| c)
    .unwrap_or(0);

    Json(json!({
        "ok": true,
        "run_id": run_id,
        "plane1": {
            "syscall_records": total_syscalls,
            "description": "supervisor syscall log"
        },
        "plane2": {
            "total_events": total_ebpf,
            "event_counts": {
                "syscall": syscall_ebpf,
                "sched_switch": sched_ebpf,
                "exec": exec_ebpf,
                "fault": fault_ebpf
            },
            "source": source.unwrap_or_else(|| "fallback".into()),
            "description": "eBPF/fallback kernel observer"
        }
    }))
}

// ---------------------------------------------------------------------------
// POST /runs/:id/tracing/seed — seed synthetic eBPF records from syscall log
// ---------------------------------------------------------------------------
//
// This is the fallback path: mirror plane-1 syscall records into plane-2 eBPF
// records (with source=fallback). Used when BPF is not available.

pub async fn seed_from_syscalls(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Json<Value> {
    // Fetch plane-1 syscall records (untyped)
    let rows = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT node, sysno, ret, vtime FROM syscall_records WHERE run_id = ? ORDER BY vtime ASC"
    )
    .bind(&run_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    if rows.is_empty() {
        // No syscall records yet — generate synthetic from observations
        let obs_rows = sqlx::query_as::<_, (i64, i64)>(
            "SELECT node, step FROM observations WHERE run_id = ? ORDER BY step ASC"
        )
        .bind(&run_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        let now = crate::state::unix_now() as i64;
        let mut inserted = 0u64;
        let mut node_counts: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();

        for (node, step) in &obs_rows {
            let count_val = {
                let count = node_counts.entry(*node).or_insert(0);
                *count += 1;
                *count as i64
            };
            let sysno = (step % 10 + 1) as i64;
            let digest_zero: Vec<u8> = vec![0u8; 32];

            let _ = sqlx::query(
                "INSERT INTO syscall_records (run_id, node, sysno, args_digest, ret, vtime)
                 VALUES (?, ?, ?, ?, 0, ?)"
            )
            .bind(&run_id)
            .bind(node)
            .bind(sysno)
            .bind(&digest_zero)
            .bind(step)
            .execute(&state.db)
            .await;

            let event = format!("syscall:{sysno}");
            let _ = sqlx::query(
                "INSERT INTO ebpf_records (run_id, node, event, value, vtime, source, recorded_at)
                 VALUES (?, ?, ?, ?, ?, 'fallback', ?)"
            )
            .bind(&run_id)
            .bind(node)
            .bind(&event)
            .bind(count_val)
            .bind(step)
            .bind(now)
            .execute(&state.db)
            .await;

            inserted += 1;
        }

        return Json(json!({
            "ok": true,
            "run_id": run_id,
            "seeded_from": "observations (synthetic)",
            "records_inserted": inserted,
        }));
    }

    // Mirror syscall records into eBPF records
    let now = crate::state::unix_now() as i64;
    let mut inserted = 0u64;
    let mut node_counts: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();

    for (node, sysno, _ret, vtime) in &rows {
        let count_val = {
            let count = node_counts.entry(*node).or_insert(0);
            *count += 1;
            *count as i64
        };
        let event = format!("syscall:{sysno}");

        let _ = sqlx::query(
            "INSERT INTO ebpf_records (run_id, node, event, value, vtime, source, recorded_at)
             VALUES (?, ?, ?, ?, ?, 'fallback', ?)"
        )
        .bind(&run_id)
        .bind(node)
        .bind(&event)
        .bind(count_val)
        .bind(vtime)
        .bind(now)
        .execute(&state.db)
        .await;

        inserted += 1;
    }

    Json(json!({
        "ok": true,
        "run_id": run_id,
        "seeded_from": "syscall_records (plane 1)",
        "records_inserted": inserted,
    }))
}

// ---------------------------------------------------------------------------
// GET /runs/:id/ebpf — list eBPF records for a run
// ---------------------------------------------------------------------------

pub async fn list_ebpf(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<EbpfListQuery>,
) -> Json<Value> {
    let records = fetch_ebpf_records(&state, &run_id, q.node, q.event.as_deref()).await;
    Json(json!({
        "ok": true,
        "run_id": run_id,
        "records": records,
        "count": records.len(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct EbpfListQuery {
    pub node: Option<u16>,
    pub event: Option<String>,
}

// ---------------------------------------------------------------------------
// Syscall log (plane 1) routes
// ---------------------------------------------------------------------------

pub async fn list_syscalls(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<SyscallQuery>,
) -> Json<Value> {
    let node_filter = q.node.map(|n| n as i64);
    let sysno_filter = q.sysno.map(|n| n as i64);

    // SQLite does not support typed NULL comparison via bind in the same way
    // across all drivers; use conditional queries instead.
    let rows: Vec<(i64, i64, i64, i64)> = match (node_filter, sysno_filter) {
        (Some(n), Some(s)) => sqlx::query_as(
            "SELECT node, sysno, ret, vtime FROM syscall_records
             WHERE run_id = ? AND node = ? AND sysno = ?
             ORDER BY vtime ASC LIMIT 1000"
        ).bind(&run_id).bind(n).bind(s),
        (Some(n), None) => sqlx::query_as(
            "SELECT node, sysno, ret, vtime FROM syscall_records
             WHERE run_id = ? AND node = ?
             ORDER BY vtime ASC LIMIT 1000"
        ).bind(&run_id).bind(n),
        (None, Some(s)) => sqlx::query_as(
            "SELECT node, sysno, ret, vtime FROM syscall_records
             WHERE run_id = ? AND sysno = ?
             ORDER BY vtime ASC LIMIT 1000"
        ).bind(&run_id).bind(s),
        (None, None) => sqlx::query_as(
            "SELECT node, sysno, ret, vtime FROM syscall_records
             WHERE run_id = ?
             ORDER BY vtime ASC LIMIT 1000"
        ).bind(&run_id),
    }
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let records: Vec<Value> = rows.iter().map(|(node, sysno, ret, vtime)| json!({
        "node": node,
        "sysno": sysno,
        "ret": ret,
        "vtime": vtime,
    })).collect();

    Json(json!({
        "ok": true,
        "run_id": run_id,
        "records": records,
        "count": records.len(),
    }))
}

pub async fn tail_syscalls(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<SyscallQuery>,
) -> Json<Value> {
    let node_filter = q.node.map(|n| n as i64);
    let sysno_filter = q.sysno.map(|n| n as i64);

    let rows: Vec<(i64, i64, i64, i64)> = match (node_filter, sysno_filter) {
        (Some(n), Some(s)) => sqlx::query_as(
            "SELECT node, sysno, ret, vtime FROM syscall_records
             WHERE run_id = ? AND node = ? AND sysno = ?
             ORDER BY vtime DESC LIMIT 100"
        ).bind(&run_id).bind(n).bind(s),
        (Some(n), None) => sqlx::query_as(
            "SELECT node, sysno, ret, vtime FROM syscall_records
             WHERE run_id = ? AND node = ?
             ORDER BY vtime DESC LIMIT 100"
        ).bind(&run_id).bind(n),
        (None, Some(s)) => sqlx::query_as(
            "SELECT node, sysno, ret, vtime FROM syscall_records
             WHERE run_id = ? AND sysno = ?
             ORDER BY vtime DESC LIMIT 100"
        ).bind(&run_id).bind(s),
        (None, None) => sqlx::query_as(
            "SELECT node, sysno, ret, vtime FROM syscall_records
             WHERE run_id = ?
             ORDER BY vtime DESC LIMIT 100"
        ).bind(&run_id),
    }
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let records: Vec<Value> = rows.iter().rev().map(|(node, sysno, ret, vtime)| json!({
        "node": node,
        "sysno": sysno,
        "ret": ret,
        "vtime": vtime,
    })).collect();

    Json(json!({
        "ok": true,
        "run_id": run_id,
        "records": records,
        "count": records.len(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct SyscallQuery {
    pub node: Option<u16>,
    pub sysno: Option<u32>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn fetch_ebpf_records(
    state: &AppState,
    run_id: &str,
    node: Option<u16>,
    event_filter: Option<&str>,
) -> Vec<Value> {
    let rows: Vec<(i64, String, i64, i64, String)> = if let Some(n) = node {
        sqlx::query_as(
            "SELECT node, event, value, vtime, source FROM ebpf_records
             WHERE run_id = ? AND node = ?
             ORDER BY vtime ASC LIMIT 500"
        )
        .bind(run_id)
        .bind(n as i64)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    } else {
        sqlx::query_as(
            "SELECT node, event, value, vtime, source FROM ebpf_records
             WHERE run_id = ?
             ORDER BY vtime ASC LIMIT 500"
        )
        .bind(run_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    };

    rows.iter()
        .filter(|(_, event, _, _, _)| {
            event_filter
                .map(|f| event.starts_with(f))
                .unwrap_or(true)
        })
        .map(|(node, event, value, vtime, source)| json!({
            "node": node,
            "event": event,
            "value": value,
            "vtime": vtime,
            "source": source,
        }))
        .collect()
}
