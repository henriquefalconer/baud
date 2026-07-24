// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /tapes — tape (sandbox) lifecycle routes
//
// Routes:
//   POST   /tapes              → create (tape create)
//   GET    /tapes              → list (tape ls)
//   GET    /tapes/:id          → status (tape status <id>)
//   POST   /tapes/:id/start    → start / ensure from stopped
//   POST   /tapes/:id/stop     → stop
//   POST   /tapes/:id/restore  → ensure from archived
//   DELETE /tapes/:id          → kill (permanent delete)
//   POST   /tapes/:id/exec     → exec command
//   GET    /tapes/:id/endpoint → preview URL

use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::AppState;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateTapeBody {
    /// Backend to use: "local" (default) or "daytona"
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Optional image/snapshot ID
    pub image: Option<String>,
}

fn default_backend() -> String {
    "local".to_owned()
}


#[derive(Debug, Deserialize)]
pub struct ExecBody {
    pub cmd: Vec<String>,
}

// ---------------------------------------------------------------------------
// POST /tapes — create a tape
// ---------------------------------------------------------------------------

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateTapeBody>,
) -> Json<Value> {
    let id = format!("tape-{}", uuid::Uuid::new_v4().to_string().replace('-', "").chars().take(12).collect::<String>());
    let now = crate::state::unix_now() as i64;

    // Create the local backend sandbox
    let backend_name = body.backend.clone();
    let tape_id = match backend_name.as_str() {
        "local" => {
            // Create a local sandbox via baud-tape-local
            match create_local_sandbox(&id, body.image.as_deref()).await {
                Ok(local_id) => local_id,
                Err(e) => {
                    return Json(json!({ "error": format!("failed to create local sandbox: {e}") }));
                }
            }
        }
        "daytona" => {
            // Daytona requires API key; return stub for now
            return Json(json!({ "error": "daytona backend requires keys init; use --backend local for development" }));
        }
        other => {
            return Json(json!({ "error": format!("unknown backend: {other}") }));
        }
    };

    // Record in SQLite
    let result = sqlx::query(
        "INSERT INTO tapes (id, backend, state, vcpus, memory_mib, disk_mib, auto_stop_secs, auto_archive_secs, image, preview_url, created_at, updated_at)
         VALUES (?, ?, 'running', 1, 1024, 1024, 60, 300, ?, NULL, ?, ?)"
    )
    .bind(&tape_id)
    .bind(&backend_name)
    .bind(&body.image)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(json!({
            "id": tape_id,
            "backend": backend_name,
            "state": "running",
            "vcpus": 1,
            "memory_mib": 1024,
            "disk_mib": 1024,
            "auto_stop_secs": 60,
            "auto_archive_secs": 300,
            "image": body.image,
            "preview_url": null,
            "created_at": now,
            "updated_at": now,
        })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

async fn create_local_sandbox(tape_id: &str, image: Option<&str>) -> Result<String, String> {
    use baud_tape_local::LocalBackend;
    use baud_tape::{Backend, types::SandboxSpec};

    let backend = LocalBackend::new();
    let spec = SandboxSpec {
        image: image.map(|s| s.to_owned()),
        ..Default::default()
    };
    backend.create(&spec).await.map_err(|e| e.to_string())?;
    // Use the tape_id as our tracking ID (the local sandbox will have its own ID)
    // For simplicity, we store our ID and manage the local backend separately
    // In a full implementation, the server would hold a reference to the backend
    Ok(tape_id.to_owned())
}

// ---------------------------------------------------------------------------
// GET /tapes — list tapes
// ---------------------------------------------------------------------------

pub async fn list(State(state): State<AppState>) -> Json<Value> {
    let rows = sqlx::query_as::<_, (String, String, String, i64, i64, i64, i64, i64, Option<String>, Option<String>, i64, i64)>(
        "SELECT id, backend, state, vcpus, memory_mib, disk_mib, auto_stop_secs, auto_archive_secs, image, preview_url, created_at, updated_at FROM tapes WHERE state != 'deleted' ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let tapes: Vec<Value> = rows.into_iter().map(|(id, backend, state_val, vcpus, mem, disk, stop, arch, image, url, ca, ua)| {
                json!({
                    "id": id,
                    "backend": backend,
                    "state": state_val,
                    "vcpus": vcpus,
                    "memory_mib": mem,
                    "disk_mib": disk,
                    "auto_stop_secs": stop,
                    "auto_archive_secs": arch,
                    "image": image,
                    "preview_url": url,
                    "created_at": ca,
                    "updated_at": ua,
                })
            }).collect();
            Json(json!({ "tapes": tapes }))
        }
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// GET /tapes/:id — status
// ---------------------------------------------------------------------------

pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    match get_tape(&state, &id).await {
        Ok(row) => row,
        Err(e) => Json(json!({ "error": e })),
    }
}

async fn get_tape(state: &AppState, id: &str) -> Result<Json<Value>, String> {
    let row = sqlx::query_as::<_, (String, String, String, i64, i64, i64, i64, i64, Option<String>, Option<String>, i64, i64)>(
        "SELECT id, backend, state, vcpus, memory_mib, disk_mib, auto_stop_secs, auto_archive_secs, image, preview_url, created_at, updated_at FROM tapes WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| format!("db error: {e}"))?;

    match row {
        Some((id, backend, state_val, vcpus, mem, disk, stop, arch, image, url, ca, ua)) => {
            Ok(Json(json!({
                "id": id,
                "backend": backend,
                "state": state_val,
                "vcpus": vcpus,
                "memory_mib": mem,
                "disk_mib": disk,
                "auto_stop_secs": stop,
                "auto_archive_secs": arch,
                "image": image,
                "preview_url": url,
                "created_at": ca,
                "updated_at": ua,
            })))
        }
        None => Err(format!("tape {id} not found")),
    }
}

// ---------------------------------------------------------------------------
// POST /tapes/:id/start — start / revive from stopped
// ---------------------------------------------------------------------------

pub async fn start(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    update_tape_state(&state, &id, "stopped", "running").await
}

// ---------------------------------------------------------------------------
// POST /tapes/:id/stop
// ---------------------------------------------------------------------------

pub async fn stop(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    update_tape_state(&state, &id, "running", "stopped").await
}

// ---------------------------------------------------------------------------
// POST /tapes/:id/restore — revive from archived
// ---------------------------------------------------------------------------

pub async fn restore(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    update_tape_state(&state, &id, "archived", "running").await
}

// ---------------------------------------------------------------------------
// POST /tapes/:id/ensure — ensure running (start if stopped, restore if archived)
// ---------------------------------------------------------------------------

pub async fn ensure(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    // Get current state
    let current = sqlx::query_as::<_, (String,)>("SELECT state FROM tapes WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await;

    match current {
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
        Ok(None) => Json(json!({ "error": format!("tape {id} not found") })),
        Ok(Some((tape_state,))) => {
            let new_state = match tape_state.as_str() {
                "running" => "running",
                "stopped" => "running",
                "archived" => "running",
                other => {
                    return Json(json!({ "error": format!("cannot ensure tape in state {other}") }));
                }
            };
            if new_state != tape_state.as_str() {
                let now = crate::state::unix_now() as i64;
                let _ = sqlx::query("UPDATE tapes SET state = ?, updated_at = ? WHERE id = ?")
                    .bind(new_state)
                    .bind(now)
                    .bind(&id)
                    .execute(&state.db)
                    .await;
            }
            match get_tape(&state, &id).await {
                Ok(r) => r,
                Err(e) => Json(json!({ "error": e })),
            }
        }
    }
}

async fn update_tape_state(state: &AppState, id: &str, _from: &str, to: &str) -> Json<Value> {
    let now = crate::state::unix_now() as i64;
    let result = sqlx::query("UPDATE tapes SET state = ?, updated_at = ? WHERE id = ?")
        .bind(to)
        .bind(now)
        .bind(id)
        .execute(&state.db)
        .await;

    match result {
        Ok(r) if r.rows_affected() == 0 => {
            Json(json!({ "error": format!("tape {id} not found") }))
        }
        Ok(_) => {
            match get_tape(state, id).await {
                Ok(r) => r,
                Err(e) => Json(json!({ "error": e })),
            }
        }
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// DELETE /tapes/:id — kill (permanent delete)
// ---------------------------------------------------------------------------

pub async fn kill(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let now = crate::state::unix_now() as i64;
    let result = sqlx::query("UPDATE tapes SET state = 'deleted', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(&id)
        .execute(&state.db)
        .await;

    match result {
        Ok(r) if r.rows_affected() == 0 => {
            Json(json!({ "error": format!("tape {id} not found") }))
        }
        Ok(_) => Json(json!({ "ok": true, "id": id, "state": "deleted" })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// POST /tapes/:id/exec — run a command in the sandbox
// ---------------------------------------------------------------------------

pub async fn exec(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ExecBody>,
) -> Json<Value> {
    // Verify tape exists and is running
    let tape = sqlx::query_as::<_, (String, String)>("SELECT id, state FROM tapes WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await;

    match tape {
        Err(e) => return Json(json!({ "error": format!("db error: {e}") })),
        Ok(None) => return Json(json!({ "error": format!("tape {id} not found") })),
        Ok(Some((_, tape_state))) if tape_state != "running" => {
            return Json(json!({ "error": format!("tape {id} is not running (state: {tape_state})") }));
        }
        Ok(_) => {}
    }

    // Execute command via local backend
    // In a full implementation, the server would look up which backend
    // manages this tape. For now, run it as a local subprocess.
    let cmd: Vec<&str> = body.cmd.iter().map(|s| s.as_str()).collect();
    if cmd.is_empty() {
        return Json(json!({ "error": "cmd must not be empty" }));
    }

    // Run the command directly (local backend stores in temp dir keyed by tape ID)
    let shell_cmd = cmd.join(" ");
    let out = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&shell_cmd)
        .output()
        .await;

    match out {
        Ok(output) => Json(json!({
            "exit_code": output.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
        })),
        Err(e) => Json(json!({ "error": format!("exec failed: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// GET /tapes/:id/endpoint — preview URL
// ---------------------------------------------------------------------------

pub async fn endpoint(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let row = sqlx::query_as::<_, (Option<String>,)>("SELECT preview_url FROM tapes WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await;

    match row {
        Ok(Some((url,))) => Json(json!({ "id": id, "url": url })),
        Ok(None) => Json(json!({ "error": format!("tape {id} not found") })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}
