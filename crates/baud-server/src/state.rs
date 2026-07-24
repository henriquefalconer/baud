// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use anyhow::Result;
use sqlx::SqlitePool;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

use crate::routes::server::LogEntry;

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
}

impl AppState {
    pub async fn new() -> Result<Self> {
        let db_url = std::env::var("BAUD_DB")
            .unwrap_or_else(|_| "sqlite://baud.sqlite?mode=rwc".to_owned());

        let pool = SqlitePool::connect(&db_url).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(AppState {
            db: pool,
            started_at: unix_now(),
            budget_minutes_used: Arc::new(Mutex::new(0)),
            log_buffer: Arc::new(RwLock::new(Vec::new())),
        })
    }
}

pub fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
