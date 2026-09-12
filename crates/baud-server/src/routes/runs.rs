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
//   POST /runs/:id/pause → pause a run
//   POST /runs/:id/resume → resume a paused run

use crate::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

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
    #[allow(dead_code)]
    pub backend: String,
}

fn default_budget() -> u64 {
    60
}
fn default_backend() -> String {
    "local".to_owned()
}

// ---------------------------------------------------------------------------
// POST /runs — start a run
// ---------------------------------------------------------------------------

type ApiError = (StatusCode, Json<Value>);

fn bad_request(message: impl Into<String>) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message.into() })),
    )
}

fn internal_error(message: impl Into<String>) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message.into() })),
    )
}

pub async fn start(
    State(state): State<AppState>,
    Json(body): Json<RunStartBody>,
) -> Result<Json<Value>, ApiError> {
    // 1. Lint the spec via baud-init. Invalid input must not be reported as an HTTP 200
    // success, because the CLI and automation use the transport status to distinguish a
    // rejected request from an accepted run.
    let spec_doc = match baud_init::lint(&body.spec) {
        Ok(doc) => doc,
        Err(e) => return Err(bad_request(format!("spec lint error: {e}"))),
    };

    // 2. Compute spec hash
    let spec_hash = format!(
        "blake3:{}",
        hex_encode(blake3::hash(body.spec.as_bytes()).as_bytes())
    );

    // 3. Compute the canonical input closure identity before journaling ownership.
    let closure_hash = compute_closure_hash(&spec_doc);

    // 4. Journal ownership before acknowledging the run. A run without a durable tape
    // reservation cannot be replayed or cancelled safely, so create both rows in one transaction.
    if body.backend != "local" && body.backend != "daytona" {
        return Err(bad_request(format!(
            "unsupported backend '{}'; expected local or daytona",
            body.backend
        )));
    }
    let run_id = format!(
        "run-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .replace('-', "")
            .chars()
            .take(12)
            .collect::<String>()
    );
    // Allocate the real backend sandbox before acknowledging the run. The backend's ID is the
    // durable tape identity, so a successful response always names a sandbox the server owns.
    if body.backend != "local" {
        return Err(bad_request(format!(
            "backend '{}' is unavailable; only the configured local backend is enabled",
            body.backend
        )));
    }
    let tape_id = match state
        .tape_backend
        .create(&baud_tape::types::SandboxSpec {
            image: Some(spec_doc.nix.clone()),
            ..Default::default()
        })
        .await
    {
        Ok(id) => id,
        Err(e) => {
            return Err(internal_error(format!(
                "backend sandbox creation failed: {e}"
            )))
        }
    };
    let now = crate::state::unix_now() as i64;
    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            let _ = state.tape_backend.delete(&tape_id).await;
            return Err(internal_error(format!("db transaction error: {e}")));
        }
    };
    let tape_result = sqlx::query(
        "INSERT INTO tapes (id, backend, state, vcpus, memory_mib, disk_mib, auto_stop_secs, auto_archive_secs, image, created_at, updated_at)
         VALUES (?, ?, 'creating', 1, 1024, 1024, 60, 300, ?, ?, ?)"
    )
    .bind(&tape_id)
    .bind(&body.backend)
    .bind(&spec_doc.nix)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await;
    if let Err(e) = tape_result {
        let _ = tx.rollback().await;
        let _ = state.tape_backend.delete(&tape_id).await;
        return Err(internal_error(format!("tape journal error: {e}")));
    }
    let result = sqlx::query(
        "INSERT INTO runs (id, spec_content, spec_hash, nix_ref, closure_hash, strategy, tactics, seed, budget_minutes, tape_id, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)"
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
    .bind(&tape_id)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await;
    match result {
        Ok(_) => {
            if let Err(e) = tx.commit().await {
                // The backend allocation happened before the transaction. If SQLite rejects the
                // commit, release that allocation before reporting failure or the accepted run
                // path leaks a live sandbox that has no durable owner.
                let _ = state.tape_backend.delete(&tape_id).await;
                return Err(internal_error(format!("db commit error: {e}")));
            }
            state.register_run(&run_id);
            let run_state = state.clone();
            let run_id_clone = run_id.clone();
            let tape_id_clone = tape_id.clone();
            let cancellation = state
                .run_cancellations
                .read()
                .expect("run cancellation registry poisoned")
                .get(&run_id)
                .cloned()
                .expect("run token registered before acknowledgement");
            tokio::spawn(async move {
                provision_run(&run_state, &run_id_clone, &tape_id_clone, cancellation).await;
                // The token must cover the whole worker lifetime, but retaining either registry
                // entry after the worker exits would let a later abort target a run that no
                // longer exists and would leak ownership metadata forever.
                run_state.remove_run(&run_id_clone);
            });
            Ok(Json(json!({
                "id": run_id,
                "tape_id": tape_id,
                "spec_hash": spec_hash,
                "nix_ref": spec_doc.nix,
                "closure_hash": closure_hash,
                "seed": body.seed,
                "budget_minutes": body.budget_minutes,
                "status": "pending",
                "nodes": spec_doc.nodes.len(),
                "created_at": now,
            })))
        }
        Err(e) => {
            let _ = state.tape_backend.delete(&tape_id).await;
            Err(internal_error(format!("db error: {e}")))
        }
    }
}

fn compute_closure_hash(spec_doc: &baud_init::parse::SpecDoc) -> String {
    // Hash the complete validated document. Files, environment, argv, and adapters all change
    // the guest inputs and therefore must change the identity recorded before acknowledgement.
    // Convert through Value so object keys, including the environment HashMap, have stable
    // ordering across independent parses and processes.
    let value = serde_json::to_value(spec_doc).expect("SpecDoc is serializable");
    let input = serde_json::to_vec(&value).expect("JSON Value is serializable");
    format!("blake3:{}", hex_encode(blake3::hash(&input).as_bytes()))
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn spec_input_hash_is_independent_of_environment_insertion_order() {
        let a = baud_init::lint("nix: test\nenv:\n  A: one\n  B: two\n").unwrap();
        let b = baud_init::lint("nix: test\nenv:\n  B: two\n  A: one\n").unwrap();
        assert_eq!(compute_closure_hash(&a), compute_closure_hash(&b));
        let changed = baud_init::lint("nix: test\nenv:\n  A: changed\n  B: two\n").unwrap();
        assert_ne!(compute_closure_hash(&a), compute_closure_hash(&changed));
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn exit_code_for_status(status: &str) -> u8 {
    match status {
        "done" | "completed" => 0,
        "crashed" | "goal" | "violation_found" => 2,
        "aborted" | "failed" | "error" | "divergent" | "pending" | "provisioning" | "running"
        | "paused" => 1,
        _ => 1,
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::exit_code_for_status;

    #[test]
    fn active_runs_are_not_successful() {
        for status in ["pending", "provisioning", "running", "paused"] {
            assert_eq!(exit_code_for_status(status), 1, "{status}");
        }
    }

    #[test]
    fn terminal_statuses_follow_the_cli_contract() {
        assert_eq!(exit_code_for_status("done"), 0);
        assert_eq!(exit_code_for_status("error"), 1);
        assert_eq!(exit_code_for_status("goal"), 2);
    }
}

/// Background task: transition the durably journaled tape and run to active ownership.
/// There is no fake sleep here. A real backend integration can replace the single transition,
/// but it must keep the run and its tape in matching states.
async fn provision_run(
    state: &AppState,
    run_id: &str,
    tape_id: &str,
    cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    if cancellation.load(std::sync::atomic::Ordering::SeqCst) {
        let now = crate::state::unix_now() as i64;
        let _ = sqlx::query("UPDATE runs SET status = 'aborted', updated_at = ? WHERE id = ? AND status IN ('pending','provisioning')")
            .bind(now)
            .bind(run_id)
            .execute(&state.db)
            .await;
        let _ = sqlx::query("UPDATE tapes SET state = 'stopped', updated_at = ? WHERE id = ? AND state = 'creating'")
            .bind(now)
            .bind(tape_id)
            .execute(&state.db)
            .await;
        state.finish_run(run_id);
        return;
    }
    let now = crate::state::unix_now() as i64;
    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            let now = crate::state::unix_now() as i64;
            let _ = sqlx::query("UPDATE runs SET status = 'failed', updated_at = ? WHERE id = ? AND status IN ('pending','provisioning')")
                .bind(now)
                .bind(run_id)
                .execute(&state.db)
                .await;
            tracing::error!(%run_id, %error, "run provisioning transaction failed");
            return;
        }
    };
    if sqlx::query("UPDATE runs SET status = 'provisioning', updated_at = ? WHERE id = ? AND status = 'pending'")
        .bind(now).bind(run_id).execute(&mut *tx).await.is_err() { return; }
    if sqlx::query(
        "UPDATE tapes SET state = 'running', updated_at = ? WHERE id = ? AND state = 'creating'",
    )
    .bind(now)
    .bind(tape_id)
    .execute(&mut *tx)
    .await
    .is_err()
    {
        return;
    }
    if sqlx::query("UPDATE runs SET status = 'running', updated_at = ? WHERE id = ? AND status = 'provisioning'")
        .bind(now).bind(run_id).execute(&mut *tx).await.is_err() { return; }
    let _ = tx.commit().await;
    state.mark_run_running(run_id);
    if cancellation.load(std::sync::atomic::Ordering::SeqCst) {
        let _ = sqlx::query("UPDATE runs SET status = 'aborted', updated_at = ? WHERE id = ? AND status = 'running'")
            .bind(crate::state::unix_now() as i64)
            .bind(run_id)
            .execute(&state.db)
            .await;
        state.finish_run(run_id);
    }
}

// ---------------------------------------------------------------------------
// GET /runs — list runs
// ---------------------------------------------------------------------------

pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query_as::<_, (String, String, String, Option<String>, i64, String, i64, i64)>(
        "SELECT id, spec_hash, nix_ref, closure_hash, seed, status, created_at, updated_at FROM runs ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let runs: Vec<Value> = rows
                .into_iter()
                .map(
                    |(id, spec_hash, nix_ref, closure_hash, seed, status, ca, ua)| {
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
                    },
                )
                .collect();
            Ok(Json(json!({ "runs": runs })))
        }
        Err(e) => Err(internal_error(format!("db error: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// GET /runs/:id — run status
// ---------------------------------------------------------------------------

pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>, i64, i64, String, i64, i64)>(
        "SELECT id, spec_hash, nix_ref, closure_hash, tape_id, seed, budget_minutes, status, created_at, updated_at FROM runs WHERE id = ?"
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    match row {
        Ok(Some((
            id,
            spec_hash,
            nix_ref,
            closure_hash,
            tape_id,
            seed,
            budget_minutes,
            status,
            ca,
            ua,
        ))) => {
            // exit_code: 0=completed, 1=active/error/aborted, 2=goal/violation (spec
            // baud-cli.md §4). An active run is not a successful completion: returning zero here
            // made polling scripts treat pending/provisioning/running as finished.
            let exit_code = exit_code_for_status(&status);
            Ok(Json(json!({
                "id": id,
                "spec_hash": spec_hash,
                "nix_ref": nix_ref,
                "closure_hash": closure_hash,
                "tape_id": tape_id,
                "seed": seed,
                "budget_minutes": budget_minutes,
                "status": status,
                "exit_code": exit_code,
                "created_at": ca,
                "updated_at": ua,
            })))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("run {id} not found") })),
        )),
        Err(e) => Err(internal_error(format!("db error: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// POST /runs/:id/pause and /runs/:id/resume — lifecycle controls
// ---------------------------------------------------------------------------

pub async fn pause(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    transition_status(&state, &id, "paused", &["running", "provisioning"]).await
}

pub async fn resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    transition_status(&state, &id, "running", &["paused"]).await
}

async fn transition_status(
    state: &AppState,
    id: &str,
    target: &str,
    allowed: &[&str],
) -> Result<Json<Value>, ApiError> {
    let now = crate::state::unix_now() as i64;
    let placeholders = std::iter::repeat("?")
        .take(allowed.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "UPDATE runs SET status = ?, updated_at = ? WHERE id = ? AND status IN ({placeholders})"
    );
    let mut query = sqlx::query(&sql).bind(target).bind(now).bind(id);
    for state_name in allowed {
        query = query.bind(state_name);
    }
    match query.execute(&state.db).await {
        Ok(result) if result.rows_affected() == 1 => {
            Ok(Json(json!({ "ok": true, "id": id, "status": target })))
        }
        Ok(_) => Err((
            StatusCode::CONFLICT,
            Json(
                json!({ "error": format!("run {id} not found or not in a {target} transition state") }),
            ),
        )),
        Err(e) => Err(internal_error(format!("db error: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// POST /runs/:id/abort — abort a run
// ---------------------------------------------------------------------------

pub async fn abort(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // Read the durable owner first. Updating only SQLite and leaving the backend sandbox running
    // made an abort look complete while the real resource remained live and could still accept
    // work after a server restart.
    let owner = sqlx::query_as::<_, (Option<String>, String)>(
        "SELECT tape_id, status FROM runs WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| internal_error(format!("db error: {error}")))?;
    let Some((tape_id, status)) = owner else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("run {id} not found") })),
        ));
    };
    if !matches!(status.as_str(), "pending" | "provisioning" | "running") {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("run {id} not found or not in an abortable state") })),
        ));
    }

    // Signal the worker before touching the backend. A KVM/replay worker may be blocked in a
    // syscall, but it now observes the same token as this request. Do not publish an aborted
    // response until the backend stop and durable transition both succeed.
    state.cancel_run(&id);
    if let Some(tape_id) = tape_id.as_deref() {
        // Ask the backend for its live state rather than trusting the SQLite copy. The worker can
        // be between the creating/running journal transitions when abort arrives.
        let tape_status = state
            .tape_backend
            .status(tape_id)
            .await
            .map_err(|error| internal_error(format!("backend status failed: {error}")))?;
        if matches!(tape_status.state, baud_tape::types::TapeState::Running) {
            state
                .tape_backend
                .stop(tape_id)
                .await
                .map_err(|error| internal_error(format!("backend stop failed: {error}")))?;
        }
    }

    let now = crate::state::unix_now() as i64;
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|error| internal_error(format!("db transaction error: {error}")))?;
    let result = sqlx::query("UPDATE runs SET status = 'aborted', updated_at = ? WHERE id = ? AND status IN ('pending','provisioning','running')")
        .bind(now)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(|error| internal_error(format!("run abort journal error: {error}")))?;
    if result.rows_affected() != 1 {
        let _ = tx.rollback().await;
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("run {id} changed state while aborting") })),
        ));
    }
    if let Some(tape_id) = tape_id {
        sqlx::query("UPDATE tapes SET state = 'stopped', updated_at = ? WHERE id = ? AND state IN ('creating','running')")
            .bind(now)
            .bind(tape_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| internal_error(format!("tape abort journal error: {error}")))?;
    }
    tx.commit()
        .await
        .map_err(|error| internal_error(format!("db commit error: {error}")))?;
    Ok(Json(json!({ "ok": true, "id": id, "status": "aborted" })))
}
