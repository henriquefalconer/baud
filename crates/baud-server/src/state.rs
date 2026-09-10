// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use anyhow::{Context, Result};
use baud_snapshot_store::SnapshotStore;
use sqlx::SqlitePool;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

use crate::routes::server::LogEntry;
use baud_tape::{
    types::{SandboxSpec, TapeState},
    Backend,
};

/// Shared application state, cloned into every request handler.
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    /// Server start time (unix seconds)
    pub started_at: u64,
    /// Accumulated sandbox-minutes consumed this session
    pub budget_minutes_used: Arc<Mutex<u64>>,
    /// In-process ring buffer of server log entries (max 4096)
    pub log_buffer: Arc<RwLock<Vec<LogEntry>>>,
    /// Durable, content-addressed, age-encrypted-at-rest universe/page/tape store
    /// (`baud-snapshot-store`) backing `/run/kvm/branch`'s `persist_run_id` and
    /// `/run/kvm/resume` (todo.md §14: "a real prerequisite for any SnapshotStore-backed
    /// resume/persist route").
    pub snapshot_store: Arc<SnapshotStore>,
    /// Long-lived sandbox backend. Keeping this instance alive is required because the local
    /// backend owns the in-memory lifecycle map; creating one per request loses the sandbox.
    pub tape_backend: Arc<dyn Backend>,
}

impl AppState {
    pub async fn new() -> Result<Self> {
        let db_url =
            std::env::var("BAUD_DB").unwrap_or_else(|_| "sqlite://baud.sqlite?mode=rwc".to_owned());

        let pool = SqlitePool::connect(&db_url).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;

        let local_backend = Arc::new(baud_tape_local::LocalBackend::new());
        let persisted_tapes = sqlx::query_as::<_, (String, String, u32, u32, u32, u32, u32, Option<String>, i64)>(
            "SELECT id, state, vcpus, memory_mib, disk_mib, auto_stop_secs, auto_archive_secs, image, created_at FROM tapes WHERE state != 'deleted'"
        )
        .fetch_all(&pool)
        .await?;
        for (
            id,
            state,
            vcpus,
            memory_mib,
            disk_mib,
            auto_stop_secs,
            auto_archive_secs,
            image,
            created_at,
        ) in persisted_tapes
        {
            let tape_state = match state.as_str() {
                "creating" => TapeState::Creating,
                "running" => TapeState::Running,
                "stopped" => TapeState::Stopped,
                "archived" => TapeState::Archived,
                other => TapeState::Unknown(other.to_owned()),
            };
            let spec = SandboxSpec {
                vcpus,
                memory_mib,
                disk_mib,
                auto_stop_secs,
                auto_archive_secs,
                image,
                ..Default::default()
            };
            if let Err(error) = local_backend
                .adopt_existing(&id, spec, tape_state, created_at as u64)
                .await
            {
                tracing::warn!(%id, %error, "could not reattach persisted local tape");
            }
        }

        Ok(AppState {
            db: pool,
            started_at: unix_now(),
            budget_minutes_used: Arc::new(Mutex::new(0)),
            log_buffer: Arc::new(RwLock::new(Vec::new())),
            snapshot_store: Arc::new(open_snapshot_store()?),
            tape_backend: local_backend,
        })
    }
}

/// Open (creating on first run) this server's `SnapshotStore`, rooted at `$BAUD_SNAPSHOT_STORE`
/// (default `baud-snapshots`, mirroring `BAUD_DB`'s own env-override convention).
///
/// Deliberately does **not** resolve its age identity via `baud_keys::age_key_path`'s OS-standard
/// `sops`/`$SOPS_AGE_KEY_FILE` search (`SnapshotStore::open`'s normal path): this dev host has
/// neither installed (`baud doctor --json` reports `age.ok=false`/`sops.ok=false`, todo.md §14),
/// and requiring an operator to run `baud keys init` before the server can even boot would make
/// every `/run/kvm/branch`/`/run/kvm/resume` call depend on external setup unrelated to KVM at
/// all. Instead this bootstraps and persists its own identity file once, under the store root
/// itself (`baud_keys::generate_identity_file`) — self-contained, and stable across restarts of
/// the same store root (the file is written once, on first use, then reused).
fn open_snapshot_store() -> Result<SnapshotStore> {
    let root = std::env::var("BAUD_SNAPSHOT_STORE").unwrap_or_else(|_| "baud-snapshots".to_owned());
    let root = std::path::PathBuf::from(root);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("creating snapshot store root {root:?}"))?;

    let identity_path = root.join(".age-identity.txt");
    if !identity_path.exists() {
        std::fs::write(&identity_path, baud_keys::generate_identity_file())
            .with_context(|| format!("writing generated age identity to {identity_path:?}"))?;
    }
    let contents = std::fs::read_to_string(&identity_path)
        .with_context(|| format!("reading age identity from {identity_path:?}"))?;
    let recipient = baud_keys::parse_public_key(&contents).with_context(|| {
        format!("identity file at {identity_path:?} has no '# public key:' line")
    })?;

    Ok(SnapshotStore::open_with_keys(
        root,
        recipient,
        Some(identity_path),
    ))
}

pub fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
