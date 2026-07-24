// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-server — local daemon
//
// Runs on the macOS dev machine (localhost only). Provides:
//   - REST + SSE endpoints for CLI (one endpoint per CLI subcommand, 1:1)
//   - SQLite metadata storage + content-addressed journal files
//   - Run orchestration and sandbox-minute budget

mod routes;
mod state;

use anyhow::Result;
use axum::{routing::get, Router};
use std::net::SocketAddr;
use tracing::info;

pub use state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("BAUD_LOG")
                .add_directive("baud_server=info".parse().unwrap()),
        )
        .init();

    let state = AppState::new().await?;
    let app = build_router(state);

    let addr: SocketAddr = "127.0.0.1:7734".parse()?;
    info!("baud-server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn build_router(state: AppState) -> Router {
    use axum::routing::{delete, post};
    Router::new()
        // Health / status
        .route("/health", get(routes::health::health))
        // Server
        .route("/server/status", get(routes::server::status))
        .route("/server/logs", get(routes::server::logs))
        // Doctor
        .route("/doctor", get(routes::doctor::doctor))
        // Keys
        .route("/keys/init", post(routes::keys::init))
        .route("/keys/show", get(routes::keys::show))
        // Tapes (M1)
        .route("/tapes", post(routes::tapes::create))
        .route("/tapes", get(routes::tapes::list))
        .route("/tapes/{id}", get(routes::tapes::status))
        .route("/tapes/{id}/start", post(routes::tapes::start))
        .route("/tapes/{id}/stop", post(routes::tapes::stop))
        .route("/tapes/{id}/restore", post(routes::tapes::restore))
        .route("/tapes/{id}/ensure", post(routes::tapes::ensure))
        .route("/tapes/{id}", delete(routes::tapes::kill))
        .route("/tapes/{id}/exec", post(routes::tapes::exec))
        .route("/tapes/{id}/endpoint", get(routes::tapes::endpoint))
        // Spec (M2)
        .route("/spec/lint", post(routes::spec::lint))
        .route("/spec/show", post(routes::spec::show))
        // Runs (M2)
        .route("/runs", post(routes::runs::start))
        .route("/runs", get(routes::runs::list))
        .route("/runs/{id}", get(routes::runs::status))
        .route("/runs/{id}/abort", post(routes::runs::abort))
        // Observations (M3: full SQLite-backed)
        .route("/runs/{id}/obs", get(routes::obs::list))
        .route("/runs/{id}/obs", post(routes::obs::append))
        .route("/runs/{id}/obs/tail", get(routes::obs::tail))
        // Verify (M3)
        .route("/verify/determinism", post(routes::verify::determinism))
        .route("/verify/determinism/poisoned", post(routes::verify::determinism_poisoned))
        .route("/verify/observation/{id}", get(routes::verify::observation))
        // Replay (M3)
        .route("/replay/{id}", post(routes::replay::replay))
        // Fuzz (M4)
        .route("/runs/fuzz", post(routes::fuzz::start))
        .route("/runs/fuzz/{id}", get(routes::fuzz::get_session))
        // Stream — frame records (M5)
        .route("/runs/{id}/frames", get(routes::stream::list_frames))
        .route("/runs/{id}/frames", post(routes::stream::append_frame))
        .route("/runs/{id}/stream/render", post(routes::stream::render))
        .route("/runs/{id}/stream/tail", get(routes::stream::tail))
        // Net weather (M5)
        .route("/runs/{id}/net/weather", get(routes::net::weather))
        .route("/runs/{id}/net/weather", post(routes::net::append_event))
        .route("/runs/{id}/net/simulate", post(routes::net::simulate_weather))
        // Raftlet (M6)
        .route("/runs/raftlet/fuzz", post(routes::raftlet::fuzz))
        .route("/runs/raftlet/{id}", get(routes::raftlet::status))
        .route("/runs/{id}/raftlet/reconstruct", post(routes::raftlet::reconstruct))
        // Tracing — plane 2 (M7)
        .route("/tracing/tail", get(routes::tracing::tail))
        .route("/tracing/summary", get(routes::tracing::summary))
        .route("/runs/{id}/tracing/seed", post(routes::tracing::seed_from_syscalls))
        .route("/runs/{id}/ebpf", get(routes::tracing::list_ebpf))
        // Syscall log — plane 1 (M7)
        .route("/runs/{id}/syscalls", get(routes::tracing::list_syscalls))
        .route("/runs/{id}/syscalls/tail", get(routes::tracing::tail_syscalls))
        // Budget
        .route("/budget", get(routes::budget::budget))
        .with_state(state)
}

pub fn router_for_tests(state: AppState) -> Router {
    build_router(state)
}
