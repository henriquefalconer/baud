// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-server — local daemon
//
// Runs on the macOS dev machine (localhost only). Provides:
//   - REST + SSE endpoints for CLI (one endpoint per CLI subcommand, 1:1)
//   - SQLite metadata storage + content-addressed journal files
//   - Run orchestration and sandbox-minute budget

#[cfg(target_os = "linux")]
mod cpu_affinity;
#[cfg(target_os = "linux")]
mod rdseed_sites;
mod routes;
mod state;

use anyhow::Result;
use axum::{
    extract::Request,
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::net::SocketAddr;
use tracing::info;

pub use state::AppState;

fn main() -> Result<()> {
    #[cfg(target_os = "linux")]
    let pinned_cpus = cpu_affinity::pin_to_quiet_cpus()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("BAUD_LOG")
                .add_directive("baud_server=info".parse().unwrap())
                // Without this, a real KVM run's progress logging (todo.md §14 item 15,
                // `run_to_first_halt_with_periodic_timer_and_devices`'s per-100-tick `info!`) is
                // silently dropped by the default filter's `ERROR`-only base level: `baud_server`
                // was the only target ever raised to `info`, so a long H9-style boot attempt would
                // still be a total black box in the server's own log even after that logging was
                // added, defeating its purpose.
                .add_directive("baud_multiverse=info".parse().unwrap())
                // Same gap, one crate over: the per-tick wall-clock `Watchdog`'s own `tracing::
                // warn!` (todo.md §14 item 16, `crates/baud-vcpu/src/linux/watchdog.rs`) lives in
                // `baud_vcpu`, which this filter never raised above the default `ERROR`-only base
                // level -- a real watchdog kill during an H9-style attempt would fire silently in
                // the server's own log with no trace of why the run failed.
                .add_directive("baud_vcpu=info".parse().unwrap()),
        )
        .init();

    #[cfg(target_os = "linux")]
    info!(
        ?pinned_cpus,
        "baud-server pinned to dynamically selected CPUs"
    );

    // Apply affinity before creating any runtime threads so workers, blocking tasks,
    // and their children inherit the same two-CPU mask.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(serve())
}

async fn serve() -> Result<()> {
    let state = AppState::new().await?;
    let app = build_router(state);

    // `BAUD_ADDR` overrides the default listen address. The default is the hardcoded
    // `127.0.0.1:7734` every CLI/drive script has always used, so unset behaves exactly as before;
    // the override exists so several `drive/*.sh` runs can spawn their own server concurrently,
    // each on its own port, instead of colliding on the single fixed one.
    let addr: SocketAddr = std::env::var("BAUD_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:7734".to_owned())
        .parse()?;
    if !addr.ip().is_loopback() {
        anyhow::bail!(
            "BAUD_ADDR must use a loopback address; refusing to expose the daemon on {addr}"
        );
    }
    info!("baud-server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn build_router(state: AppState) -> Router {
    use axum::routing::{delete, post};
    let router = Router::new()
        // Health / status
        .route("/health", get(routes::health::health))
        // Server
        .route("/server/status", get(routes::server::status))
        .route("/server/logs", get(routes::server::logs))
        // Doctor
        .route("/doctor", get(routes::doctor::doctor))
        // Host (H0 capability spike, specs/baud-host.md)
        .route("/host/probe", get(routes::host::probe))
        // Image (guest-image contract, todo.md §4, specs/baud-packages.md §9)
        .route("/image/lint", post(routes::image::lint))
        .route("/image/rewrite-rdseed", post(routes::image::rewrite_rdseed))
        .route("/image/build", post(routes::image::build))
        // Keys
        .route("/keys/init", post(routes::keys::init))
        .route("/keys/show", get(routes::keys::show))
        .route("/keys/rotate", post(routes::keys::rotate))
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
        .route("/tapes/{id}/probe-caps", get(routes::tapes::endpoint))
        .route("/tapes/{id}/reconstruct", post(routes::tapes::reconstruct))
        // Spec (M2)
        .route("/spec/lint", post(routes::spec::lint))
        .route("/spec/show", post(routes::spec::show))
        // Runs (M2)
        .route("/runs", post(routes::runs::start))
        .route("/runs", get(routes::runs::list))
        .route("/runs/{id}", get(routes::runs::status))
        .route("/runs/{id}/abort", post(routes::runs::abort))
        .route("/runs/{id}/pause", post(routes::runs::pause))
        .route("/runs/{id}/resume", post(routes::runs::resume))
        // Observations (M3: full SQLite-backed)
        .route("/runs/{id}/obs", get(routes::obs::list))
        .route("/runs/{id}/obs", post(routes::obs::append))
        .route("/runs/{id}/obs/tail", get(routes::obs::tail))
        // Keep the public command names and route names aligned. `run watch` is the live
        // observation stream, while `obs get` is the same durable list with an explicit alias.
        .route("/runs/{id}/watch", get(routes::obs::tail))
        .route("/runs/{id}/observations", get(routes::obs::list))
        // Verify (M3)
        .route("/verify/determinism", post(routes::verify::determinism))
        .route(
            "/verify/determinism/poisoned",
            post(routes::verify::determinism_poisoned),
        )
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
        .route(
            "/runs/{id}/net/simulate",
            post(routes::net::simulate_weather),
        )
        // Tracing — plane 2 (M7)
        .route("/tracing/tail", get(routes::tracing::tail))
        .route("/tracing/summary", get(routes::tracing::summary))
        .route(
            "/runs/{id}/tracing/seed",
            post(routes::tracing::seed_from_syscalls),
        )
        .route("/runs/{id}/ebpf", get(routes::tracing::list_ebpf))
        // Syscall log — plane 1 (M7)
        .route("/runs/{id}/syscalls", get(routes::tracing::list_syscalls))
        .route(
            "/runs/{id}/syscalls/get",
            get(routes::tracing::list_syscalls),
        )
        .route(
            "/runs/{id}/syscalls/tail",
            get(routes::tracing::tail_syscalls),
        )
        // Budget (M9)
        .route("/budget", get(routes::budget::budget))
        .route("/budget/record", post(routes::budget::record))
        // Shrink (M9)
        .route("/runs/{id}/shrink", post(routes::shrink::shrink))
        .route("/runs/{id}/shrink", get(routes::shrink::get_shrink));
    add_run_kvm_route(router)
        .layer(middleware::from_fn(require_configured_token))
        .with_state(state)
}

/// Protect the daemon when `BAUD_AUTH_TOKEN` is configured. Local development keeps the historical
/// unauthenticated mode when the variable is absent, while deployed and end-to-end authenticated
/// runs get one check covering REST, SSE, and WebSocket routes alike.
async fn require_configured_token(request: Request, next: Next) -> Response {
    let expected = std::env::var_os("BAUD_AUTH_TOKEN");
    let identity_seed = std::env::var("BAUD_IDENTITY_SEED_B64").ok();
    if expected.is_none() && identity_seed.is_none() {
        return next.run(request).await;
    }
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }
    let expected = expected.map(|value| value.to_string_lossy().into_owned());
    let supplied = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if authorization_matches(
        supplied,
        expected.as_deref().map(std::borrow::Cow::Borrowed),
        identity_seed.as_deref(),
    ) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [("content-type", "application/json")],
            r#"{"error":"authentication required"}"#,
        )
            .into_response()
    }
}

fn authorization_matches(
    header: Option<&str>,
    expected: Option<std::borrow::Cow<'_, str>>,
    identity_seed: Option<&str>,
) -> bool {
    let Some(token) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
        return false;
    };
    if token.is_empty() {
        return false;
    }
    // BAUD_AUTH_TOKEN remains a local-development credential. Deployed servers can instead
    // verify the signed ten-minute agent token minted by baud-identity. Invalid seed material
    // fails closed, and the token itself is never logged.
    if expected
        .as_deref()
        .is_some_and(|configured| !configured.is_empty() && token == configured)
    {
        return true;
    }
    identity_seed
        .and_then(|seed| baud_identity::RootKey::from_seed_b64(seed).ok())
        .and_then(|root| root.verify(token).ok())
        .is_some()
}

#[cfg(test)]
mod auth_tests {
    use super::authorization_matches;

    #[test]
    fn bearer_auth_requires_exact_token() {
        assert!(authorization_matches(
            Some("Bearer secret"),
            Some("secret".into()),
            None
        ));
        assert!(!authorization_matches(
            Some("Basic secret"),
            Some("secret".into()),
            None
        ));
        assert!(!authorization_matches(
            Some("Bearer secret-extra"),
            Some("secret".into()),
            None
        ));
        assert!(!authorization_matches(None, Some("secret".into()), None));
        assert!(!authorization_matches(
            Some("Bearer "),
            Some("".into()),
            None
        ));
        assert!(!authorization_matches(
            Some("Bearer "),
            Some("secret".into()),
            None
        ));
    }

    #[test]
    fn signed_identity_tokens_are_accepted_and_invalid_tokens_are_rejected() {
        let (root, seed) = baud_identity::RootKey::generate().unwrap();
        let token = root.mint_tape_token("sandbox", "run").unwrap();
        let token = token.expose().to_owned();
        assert!(authorization_matches(
            Some(&format!("Bearer {token}")),
            None,
            Some(seed.expose()),
        ));
        assert!(!authorization_matches(
            Some("Bearer invalid"),
            None,
            Some(seed.expose()),
        ));
    }
}

// Run/kvm — boot a guest on the real post-pivot KVM Multiverse (H0-H6, todo.md §14's "every
// existing route still imports the old pre-pivot Multiverse" gap). Linux-only, like the module
// it calls (`baud_multiverse::linux` is itself `#[cfg(target_os = "linux")]`).
#[cfg(target_os = "linux")]
fn add_run_kvm_route(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/run/kvm", axum::routing::post(routes::run_kvm::run))
        .route(
            "/run/kvm/branch",
            axum::routing::post(routes::run_kvm::branch),
        )
        .route(
            "/run/kvm/resume",
            axum::routing::post(routes::run_kvm::resume),
        )
        .route(
            "/shell-into/{run_id}/{node_id}",
            axum::routing::get(routes::shell_into::shell_into),
        )
        // Verify — fingerprint (H9, todo.md §14 item 9): needs the real KVM Multiverse, like
        // every other route in this Linux-only group.
        .route(
            "/verify/fingerprint",
            axum::routing::post(routes::verify_fingerprint::fingerprint),
        )
}

#[cfg(not(target_os = "linux"))]
fn add_run_kvm_route(router: Router<AppState>) -> Router<AppState> {
    router
}

pub fn router_for_tests(state: AppState) -> Router {
    build_router(state)
}
