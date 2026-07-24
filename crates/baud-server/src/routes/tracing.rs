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
        // Look up runs by tape_id
        sqlx::query_scalar!(
            "SELECT id FROM runs WHERE tape_id = ? ORDER BY created_at DESC LIMIT 1",
            tape_id
        )
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None)
        .flatten()
    } else {
        None
    };

    let (records, run_id_display): (Vec<Value>, String) = if let Some(rid) = run_filter {
        let recs = fetch_ebpf_records(&state, &rid, q.node, q.event.as_deref()).await;
        (recs, rid)
    } else {
        // No tape filter: return most recent 50 records across all runs
        let rows = sqlx::query!(
            "SELECT run_id, node, event, value, vtime, source FROM ebpf_records
             ORDER BY recorded_at DESC LIMIT 50"
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        let recs: Vec<Value> = rows.iter().map(|r| json!({
            "run_id": r.run_id,
            "node": r.node,
            "event": r.event,
            "value": r.value,
            "vtime": r.vtime,
            "source": r.source,
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
    let run_exists = sqlx::query!("SELECT id FROM runs WHERE id = ?", run_id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None)
        .is_some();

    if !run_exists {
        return Json(json!({ "ok": false, "error": format!("run not found: {run_id}") }));
    }

    // Aggregate eBPF records
    let total_ebpf: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ?", run_id
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    let syscall_ebpf: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ? AND event LIKE 'syscall:%'", run_id
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    let sched_ebpf: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ? AND event LIKE 'sched_switch%'", run_id
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    let exec_ebpf: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ? AND event = 'exec'", run_id
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    let fault_ebpf: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM ebpf_records WHERE run_id = ? AND event = 'fault'", run_id
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // Source
    let source: Option<String> = sqlx::query_scalar!(
        "SELECT source FROM ebpf_records WHERE run_id = ? LIMIT 1", run_id
    )
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    // Syscall log (plane 1) count
    let total_syscalls: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM syscall_records WHERE run_id = ?", run_id
    )
    .fetch_one(&state.db)
    .await
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
// In a real deployment this would be driven by the aya probe ringbuf.

pub async fn seed_from_syscalls(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Json<Value> {
    // Fetch plane-1 syscall records
    let rows = sqlx::query!(
        "SELECT node, sysno, ret, vtime FROM syscall_records WHERE run_id = ? ORDER BY vtime ASC",
        run_id
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    if rows.is_empty() {
        // No syscall records yet — generate synthetic from observations
        let obs_rows = sqlx::query!(
            "SELECT node, step FROM observations WHERE run_id = ? ORDER BY step ASC",
            run_id
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        // Generate one synthetic syscall record per observation step per node
        let now = crate::state::unix_now() as i64;
        let mut inserted = 0u64;
        let mut node_counts: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();

        for obs in &obs_rows {
            let node = obs.node;
            let step = obs.step;
            let count_val = {
                let count = node_counts.entry(node).or_insert(0);
                *count += 1;
                *count as i64
            };
            let sysno = (step % 10 + 1) as i64;
            let digest_zero: Vec<u8> = vec![0u8; 32];

            // Insert syscall record (plane 1) — no recorded_at (not in original schema)
            let _ = sqlx::query!(
                "INSERT INTO syscall_records (run_id, node, sysno, args_digest, ret, vtime)
                 VALUES (?, ?, ?, ?, 0, ?)",
                run_id, node, sysno, digest_zero, step
            )
            .execute(&state.db)
            .await;

            // Insert corresponding eBPF record (plane 2, fallback)
            let event = format!("syscall:{sysno}");
            let _ = sqlx::query!(
                "INSERT INTO ebpf_records (run_id, node, event, value, vtime, source, recorded_at)
                 VALUES (?, ?, ?, ?, ?, 'fallback', ?)",
                run_id, node, event, count_val, step, now
            )
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

    for row in &rows {
        let count_val = {
            let count = node_counts.entry(row.node).or_insert(0);
            *count += 1;
            *count as i64
        };
        let event = format!("syscall:{}", row.sysno);

        let _ = sqlx::query!(
            "INSERT INTO ebpf_records (run_id, node, event, value, vtime, source, recorded_at)
             VALUES (?, ?, ?, ?, ?, 'fallback', ?)",
            run_id, row.node, event, count_val, row.vtime, now
        )
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

    let rows = sqlx::query!(
        "SELECT node, sysno, ret, vtime FROM syscall_records
         WHERE run_id = ?
           AND (? IS NULL OR node = ?)
           AND (? IS NULL OR sysno = ?)
         ORDER BY vtime ASC LIMIT 1000",
        run_id,
        node_filter, node_filter,
        sysno_filter, sysno_filter,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let records: Vec<Value> = rows.iter().map(|r| json!({
        "node": r.node,
        "sysno": r.sysno,
        "ret": r.ret,
        "vtime": r.vtime,
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
    // Same as list_syscalls but last 100
    let node_filter = q.node.map(|n| n as i64);
    let sysno_filter = q.sysno.map(|n| n as i64);

    let rows = sqlx::query!(
        "SELECT node, sysno, ret, vtime FROM syscall_records
         WHERE run_id = ?
           AND (? IS NULL OR node = ?)
           AND (? IS NULL OR sysno = ?)
         ORDER BY vtime DESC LIMIT 100",
        run_id,
        node_filter, node_filter,
        sysno_filter, sysno_filter,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let records: Vec<Value> = rows.iter().rev().map(|r| json!({
        "node": r.node,
        "sysno": r.sysno,
        "ret": r.ret,
        "vtime": r.vtime,
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
    let node_i = node.map(|n| n as i64);
    let rows = sqlx::query!(
        "SELECT node, event, value, vtime, source FROM ebpf_records
         WHERE run_id = ?
           AND (? IS NULL OR node = ?)
         ORDER BY vtime ASC LIMIT 500",
        run_id,
        node_i, node_i,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    rows.iter()
        .filter(|r| {
            event_filter
                .map(|f| r.event.starts_with(f))
                .unwrap_or(true)
        })
        .map(|r| json!({
            "node": r.node,
            "event": r.event,
            "value": r.value,
            "vtime": r.vtime,
            "source": r.source,
        }))
        .collect()
}
