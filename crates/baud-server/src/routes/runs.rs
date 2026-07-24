// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /runs — run lifecycle routes (M2)
//
// Routes:
//   POST /runs         → start a run (run start)
//   GET  /runs         → list runs (run ls)
//   GET  /runs/:id     → run status
//   POST /runs/:id/abort → abort a run

use axum::{extract::{Path, State}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RunStartBody {
    /// Raw spec content (YAML)
    pub spec: String,
    /// Optional strategy spec
    pub strategy: Option<String>,
    /// Optional tactics spec
    pub tactics: Option<String>,
    /// RNG seed
    #[serde(default)]
    pub seed: u64,
    /// Budget in minutes
    #[serde(default = "default_budget")]
    pub budget_minutes: u64,
    /// Backend to use: "local" (default) or "daytona"
    #[serde(default = "default_backend")]
    pub backend: String,
}

fn default_budget() -> u64 { 60 }
fn default_backend() -> String { "local".to_owned() }

// ---------------------------------------------------------------------------
// POST /runs — start a run
// ---------------------------------------------------------------------------

pub async fn start(
    State(state): State<AppState>,
    Json(body): Json<RunStartBody>,
) -> Json<Value> {
    // 1. Lint the spec via baud-init
    let spec_doc = match baud_init::lint(&body.spec) {
        Ok(doc) => doc,
        Err(e) => return Json(json!({ "error": format!("spec lint error: {e}") })),
    };

    // 2. Compute spec hash
    let spec_hash = format!("blake3:{}", hex_encode(blake3::hash(body.spec.as_bytes()).as_bytes()));

    // 3. Compute closure hash via baud-packages (stub if nix not available)
    let closure_hash = compute_closure_hash(&spec_doc);

    // 4. Create a run record
    let run_id = format!("run-{}", uuid::Uuid::new_v4().to_string().replace('-', "").chars().take(12).collect::<String>());
    let now = crate::state::unix_now() as i64;

    let result = sqlx::query(
        "INSERT INTO runs (id, spec_content, spec_hash, nix_ref, closure_hash, strategy, tactics, seed, budget_minutes, tape_id, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 'pending', ?, ?)"
    )
    .bind(&run_id)
    .bind(&body.spec)
    .bind(&spec_hash)
    .bind(&spec_doc.nix)
    .bind(&closure_hash)
    .bind(body.strategy.as_deref())
    .bind(body.tactics.as_deref())
    .bind(body.seed as i64)
    .bind(body.budget_minutes as i64)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => {
            // Spawn provisioning in background (non-blocking)
            let db = state.db.clone();
            let run_id_clone = run_id.clone();
            tokio::spawn(async move {
                provision_run(&db, &run_id_clone).await;
            });

            Json(json!({
                "id": run_id,
                "spec_hash": spec_hash,
                "nix_ref": spec_doc.nix,
                "closure_hash": closure_hash,
                "seed": body.seed,
                "budget_minutes": body.budget_minutes,
                "status": "pending",
                "nodes": spec_doc.nodes.len(),
                "created_at": now,
            }))
        }
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

fn compute_closure_hash(spec_doc: &baud_init::parse::SpecDoc) -> String {
    // Build a deterministic closure hash from the spec.
    // In a full implementation, this would call baud-packages::build().
    // For now: hash the nix ref + node names as a stable placeholder.
    let input = format!(
        "{}:{}",
        spec_doc.nix,
        spec_doc.nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>().join(",")
    );
    format!("blake3:{}", hex_encode(blake3::hash(input.as_bytes()).as_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Background task: transition run from pending → provisioning → running
async fn provision_run(db: &sqlx::SqlitePool, run_id: &str) {
    let now = crate::state::unix_now() as i64;
    // Move to provisioning
    let _ = sqlx::query("UPDATE runs SET status = 'provisioning', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(run_id)
        .execute(db)
        .await;

    // Simulate provisioning delay (in a real impl: create tape, upload spec, start agent)
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let now = crate::state::unix_now() as i64;
    let _ = sqlx::query("UPDATE runs SET status = 'running', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(run_id)
        .execute(db)
        .await;
}

// ---------------------------------------------------------------------------
// GET /runs — list runs
// ---------------------------------------------------------------------------

pub async fn list(State(state): State<AppState>) -> Json<Value> {
    let rows = sqlx::query_as::<_, (String, String, String, Option<String>, i64, String, i64, i64)>(
        "SELECT id, spec_hash, nix_ref, closure_hash, seed, status, created_at, updated_at FROM runs ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let runs: Vec<Value> = rows.into_iter().map(|(id, spec_hash, nix_ref, closure_hash, seed, status, ca, ua)| {
                json!({
                    "id": id,
                    "spec_hash": spec_hash,
                    "nix_ref": nix_ref,
                    "closure_hash": closure_hash,
                    "seed": seed,
                    "status": status,
                    "created_at": ca,
                    "updated_at": ua,
                })
            }).collect();
            Json(json!({ "runs": runs }))
        }
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// GET /runs/:id — run status
// ---------------------------------------------------------------------------

pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let row = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>, i64, i64, i64, String, i64, i64)>(
        "SELECT id, spec_hash, nix_ref, closure_hash, tape_id, seed, budget_minutes, budget_minutes, status, created_at, updated_at FROM runs WHERE id = ?"
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    match row {
        Ok(Some((id, spec_hash, nix_ref, closure_hash, tape_id, seed, budget_minutes, _b2, status, ca, ua))) => {
            Json(json!({
                "id": id,
                "spec_hash": spec_hash,
                "nix_ref": nix_ref,
                "closure_hash": closure_hash,
                "tape_id": tape_id,
                "seed": seed,
                "budget_minutes": budget_minutes,
                "status": status,
                "created_at": ca,
                "updated_at": ua,
            }))
        }
        Ok(None) => Json(json!({ "error": format!("run {id} not found") })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// POST /runs/:id/abort — abort a run
// ---------------------------------------------------------------------------

pub async fn abort(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let now = crate::state::unix_now() as i64;
    let result = sqlx::query("UPDATE runs SET status = 'aborted', updated_at = ? WHERE id = ? AND status IN ('pending','provisioning','running')")
        .bind(now)
        .bind(&id)
        .execute(&state.db)
        .await;

    match result {
        Ok(r) if r.rows_affected() == 0 => {
            Json(json!({ "error": format!("run {id} not found or not in an abortable state") }))
        }
        Ok(_) => Json(json!({ "ok": true, "id": id, "status": "aborted" })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}
