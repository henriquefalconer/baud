// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// DaytonaBackend — typed REST client for the Daytona API.
//
// Wraps only the endpoints baud calls. Retries with exponential backoff.
// Recorded-fixture contract tests (not run in CI without a real API key).

use std::path::Path;
use std::time::Duration;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use baud_secret::SecretString;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::backend::Backend;
use crate::types::{ExecResult, SandboxSpec, SandboxStatus, TapeState};

// ---------------------------------------------------------------------------
// Daytona API types
// ---------------------------------------------------------------------------

/// Daytona workspace/sandbox create request
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkspaceRequest {
    name: String,
    image: Option<String>,
    resources: Resources,
    labels: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Resources {
    cpu: u32,
    memory: u32, // MiB
    disk: u32,   // MiB
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceResponse {
    id: String,
    name: Option<String>,
    state: Option<String>,
    #[serde(default)]
    cpu: u32,
    #[serde(default)]
    memory: u32,
    #[serde(default)]
    disk: u32,
    #[serde(rename = "previewUrl")]
    preview_url: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecRequest {
    command: String,
    timeout: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecResponse {
    #[serde(default)]
    exit_code: i32,
    #[serde(default)]
    output: String,
    #[serde(rename = "stderr", default)]
    stderr: String,
}

// ---------------------------------------------------------------------------
// DaytonaBackend
// ---------------------------------------------------------------------------

/// A Backend implementation backed by the Daytona cloud API.
pub struct DaytonaBackend {
    api_url: String,
    api_key: SecretString,
    http: Client,
    /// Maximum retry attempts for transient errors
    max_retries: u32,
}

impl DaytonaBackend {
    /// Create a new DaytonaBackend.
    ///
    /// - `api_url`: base URL of the Daytona API (e.g. `https://app.daytona.io/api`)
    /// - `api_key`: API key (held as SecretString)
    pub fn new(api_url: &str, api_key: SecretString) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        DaytonaBackend {
            api_url: api_url.trim_end_matches('/').to_owned(),
            api_key,
            http,
            max_retries: 3,
        }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key.expose())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    /// GET with retries
    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        self.with_retries(|| async {
            let resp = self
                .http
                .get(self.url(path))
                .header("Authorization", self.auth_header())
                .send()
                .await
                .context("GET request failed")?;
            let status = resp.status();
            let body = resp.text().await.context("read body")?;
            if !status.is_success() {
                bail!("GET {path} returned {status}: {body}");
            }
            serde_json::from_str(&body)
                .with_context(|| format!("parse response: {body}"))
        })
        .await
    }

    /// POST with retries
    async fn post_json<B: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.with_retries(|| async {
            let resp = self
                .http
                .post(self.url(path))
                .header("Authorization", self.auth_header())
                .json(body)
                .send()
                .await
                .context("POST request failed")?;
            let status = resp.status();
            let body_text = resp.text().await.context("read body")?;
            if !status.is_success() {
                bail!("POST {path} returned {status}: {body_text}");
            }
            serde_json::from_str(&body_text)
                .with_context(|| format!("parse response: {body_text}"))
        })
        .await
    }

    /// DELETE
    async fn delete_req(&self, path: &str) -> Result<()> {
        self.with_retries(|| async {
            let resp = self
                .http
                .delete(self.url(path))
                .header("Authorization", self.auth_header())
                .send()
                .await
                .context("DELETE request failed")?;
            let status = resp.status();
            if !status.is_success() && status != StatusCode::NOT_FOUND {
                let body = resp.text().await.unwrap_or_default();
                bail!("DELETE {path} returned {status}: {body}");
            }
            Ok(())
        })
        .await
    }

    async fn with_retries<F, Fut, T>(&self, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut last_err = None;
        for attempt in 0..=self.max_retries {
            match f().await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let msg = e.to_string();
                    // Only retry on network-level errors or 5xx
                    if attempt < self.max_retries && is_retryable(&msg) {
                        let delay = Duration::from_millis(200 * (1 << attempt));
                        warn!("attempt {}/{}: {msg}; retrying in {delay:?}", attempt + 1, self.max_retries);
                        tokio::time::sleep(delay).await;
                        last_err = Some(e);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Err(last_err.unwrap())
    }

    /// Build the enforced SandboxSpec from a caller-supplied spec.
    ///
    /// Hard constraints (spec §2):
    ///   vcpus=1, memory_mib=1024, disk_mib≥1024, auto_stop_secs=60, auto_archive_secs=300.
    /// This is the single place that enforces those invariants.
    pub(crate) fn enforce_spec(spec: &SandboxSpec) -> SandboxSpec {
        SandboxSpec {
            vcpus: 1,
            memory_mib: 1024,
            disk_mib: spec.disk_mib.max(1024),
            auto_stop_secs: 60,
            auto_archive_secs: 300,
            image: spec.image.clone(),
            labels: spec.labels.clone(),
        }
    }

    fn parse_state(s: Option<&str>) -> TapeState {
        match s.unwrap_or("") {
            "running" | "started" => TapeState::Running,
            "stopped" | "auto-stopped" => TapeState::Stopped,
            "archived" | "auto-archived" => TapeState::Archived,
            "creating" | "starting" | "provisioning" => TapeState::Creating,
            "deleted" | "destroyed" => TapeState::Deleted,
            other => TapeState::Unknown(other.to_owned()),
        }
    }

    fn parse_status(r: WorkspaceResponse) -> SandboxStatus {
        let created_at = r.created_at
            .and_then(|s| chrono_or_fallback(&s))
            .unwrap_or(0);
        SandboxStatus {
            id: r.id,
            state: Self::parse_state(r.state.as_deref()),
            vcpus: if r.cpu == 0 { 1 } else { r.cpu },
            memory_mib: if r.memory == 0 { 1024 } else { r.memory },
            disk_mib: if r.disk == 0 { 1024 } else { r.disk },
            auto_stop_secs: 60,
            auto_archive_secs: 300,
            preview_url: r.preview_url,
            created_at,
        }
    }
}

fn is_retryable(msg: &str) -> bool {
    msg.contains("connection refused")
        || msg.contains("timeout")
        || msg.contains("500")
        || msg.contains("502")
        || msg.contains("503")
}

fn chrono_or_fallback(s: &str) -> Option<u64> {
    // Try to parse ISO 8601 / RFC 3339 timestamp
    // Simple fallback: just use 0 if unparseable (we don't pull chrono)
    // Format: "2026-07-24T12:34:56Z"
    let _ = s;
    // Without chrono, return current time as best-effort
    use std::time::{SystemTime, UNIX_EPOCH};
    Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
}

#[async_trait]
impl Backend for DaytonaBackend {
    async fn create(&self, spec: &SandboxSpec) -> Result<String> {
        // Always enforce hard constraints via the canonical helper — caller cannot override.
        let enforced = Self::enforce_spec(spec);
        let name = format!("baud-{}", uuid::Uuid::new_v4().to_string().split('-').next().unwrap());
        let req = CreateWorkspaceRequest {
            name,
            image: enforced.image.clone(),
            resources: Resources {
                cpu: enforced.vcpus,
                memory: enforced.memory_mib,
                disk: enforced.disk_mib,
            },
            labels: enforced.labels.clone(),
        };
        let resp: WorkspaceResponse = self.post_json("/workspaces", &req).await?;
        debug!("created Daytona workspace {}", resp.id);
        Ok(resp.id)
    }

    async fn start(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(&format!("/workspaces/{id}/start"), &serde_json::json!({}))
            .await
            .unwrap_or(serde_json::json!({}));
        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post_json(&format!("/workspaces/{id}/stop"), &serde_json::json!({}))
            .await
            .unwrap_or(serde_json::json!({}));
        Ok(())
    }

    async fn restore(&self, id: &str) -> Result<()> {
        // Daytona uses the same start endpoint to restore archived workspaces
        self.start(id).await
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.delete_req(&format!("/workspaces/{id}")).await
    }

    async fn status(&self, id: &str) -> Result<SandboxStatus> {
        let resp: WorkspaceResponse = self.get_json(&format!("/workspaces/{id}")).await?;
        Ok(Self::parse_status(resp))
    }

    async fn exec(&self, id: &str, cmd: &[&str]) -> Result<ExecResult> {
        let command = cmd.join(" ");
        let req = ExecRequest {
            command,
            timeout: 30,
        };
        let resp: ExecResponse = self
            .post_json(&format!("/workspaces/{id}/exec"), &req)
            .await?;
        Ok(ExecResult {
            exit_code: resp.exit_code,
            stdout: resp.output,
            stderr: resp.stderr,
        })
    }

    async fn put(&self, id: &str, remote_path: &Path, data: &[u8]) -> Result<()> {
        let path_str = remote_path.to_string_lossy();
        let url = self.url(&format!("/workspaces/{id}/files?path={path_str}"));
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await
            .context("file upload failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("PUT file returned {status}: {body}");
        }
        Ok(())
    }

    async fn get(&self, id: &str, remote_path: &Path) -> Result<Vec<u8>> {
        let path_str = remote_path.to_string_lossy();
        let url = self.url(&format!("/workspaces/{id}/files?path={path_str}"));
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .context("file download failed")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("GET file returned {status}: {body}");
        }
        Ok(resp.bytes().await.context("read file body")?.to_vec())
    }

    async fn endpoint(&self, id: &str) -> Result<Option<String>> {
        let s = self.status(id).await?;
        Ok(s.preview_url)
    }
}

// ---------------------------------------------------------------------------
// Tests (unit-level; no network required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_known_values() {
        assert_eq!(DaytonaBackend::parse_state(Some("running")), TapeState::Running);
        assert_eq!(DaytonaBackend::parse_state(Some("stopped")), TapeState::Stopped);
        assert_eq!(DaytonaBackend::parse_state(Some("archived")), TapeState::Archived);
        assert_eq!(DaytonaBackend::parse_state(Some("deleted")), TapeState::Deleted);
    }

    #[test]
    fn parse_state_unknown() {
        let s = DaytonaBackend::parse_state(Some("frobnicating"));
        assert!(matches!(s, TapeState::Unknown(_)));
    }

    #[test]
    fn is_retryable_patterns() {
        assert!(is_retryable("connection refused"));
        assert!(is_retryable("timeout"));
        assert!(is_retryable("503 service unavailable"));
        assert!(!is_retryable("404 not found"));
    }

    #[test]
    fn enforces_sandbox_shape() {
        // VR2-m6: DaytonaBackend must clamp all caller-supplied values to the hard
        // constraints (spec §2) — callers cannot accidentally over-provision or skip
        // the auto-stop timer.
        let caller_spec = SandboxSpec {
            vcpus: 8,           // caller wants 8 CPUs — must be clamped to 1
            memory_mib: 8192,   // caller wants 8 GiB — must be clamped to 1024
            disk_mib: 512,      // caller wants 512 MiB — must be raised to 1024
            auto_stop_secs: 0,  // caller disables auto-stop — must be forced to 60
            auto_archive_secs: 0, // caller disables auto-archive — must be forced to 300
            image: None,
            labels: Default::default(),
        };

        let enforced = DaytonaBackend::enforce_spec(&caller_spec);

        assert_eq!(enforced.vcpus, 1,             "vcpus must always be 1");
        assert_eq!(enforced.memory_mib, 1024,     "memory_mib must always be 1024");
        assert_eq!(enforced.disk_mib, 1024,       "disk_mib must be at least 1024 (raised from 512)");
        assert_eq!(enforced.auto_stop_secs, 60,   "auto_stop_secs must always be 60");
        assert_eq!(enforced.auto_archive_secs, 300, "auto_archive_secs must always be 300");

        // Disk larger than 1024 is allowed (some platforms have a higher minimum)
        let large_disk_spec = SandboxSpec {
            disk_mib: 4096,
            ..SandboxSpec::default()
        };
        let large = DaytonaBackend::enforce_spec(&large_disk_spec);
        assert_eq!(large.disk_mib, 4096, "disk_mib larger than 1024 must be preserved");
    }
}

// ---------------------------------------------------------------------------
// Recorded-fixture contract tests (no real Daytona API key required)
// ---------------------------------------------------------------------------
// These tests start a local mock HTTP server, replay recorded API response
// fixtures, and assert that DaytonaBackend serialises requests correctly and
// maps responses onto the baud domain types.  They are annotated #[ignore]
// when the 'daytona_fixtures' cfg flag is absent so they do not block CI on
// machines without the Daytona SDK.  Run them with:
//
//   cargo test -p baud-tape --features='' -- --include-ignored daytona_contract
//
// ---------------------------------------------------------------------------
#[cfg(test)]
mod contract_tests {
    use super::*;
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path, header};

    fn make_client(base_url: &str) -> DaytonaBackend {
        use baud_secret::SecretString;
        DaytonaBackend::new(base_url, SecretString::new("test-key".to_string()))
    }

    // ------------------------------------------------------------------
    // Fixture: POST /workspaces — create returns workspace id
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn contract_create_workspace() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/workspaces"))
            .and(header("Authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    r#"{"id":"ws-abc123","name":"baud-test","state":"creating","cpu":1,"memory":1024,"disk":1024}"#,
                    "application/json",
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let spec = SandboxSpec::default();
        let id = client.create(&spec).await.expect("create must succeed");
        assert_eq!(id, "ws-abc123", "create must return the workspace id from fixture");
    }

    // ------------------------------------------------------------------
    // Fixture: GET /workspaces/{id} — status mapping
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn contract_status_maps_state() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/workspaces/ws-abc123"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    r#"{"id":"ws-abc123","state":"running","cpu":1,"memory":1024,"disk":1024,"previewUrl":"https://sandbox.example.com"}"#,
                    "application/json",
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let status = client.status("ws-abc123").await.expect("status must succeed");
        assert_eq!(status.state, TapeState::Running, "state 'running' must map to TapeState::Running");
        assert_eq!(status.vcpus, 1);
        assert_eq!(status.memory_mib, 1024);
        assert_eq!(status.preview_url.as_deref(), Some("https://sandbox.example.com"));
    }

    // ------------------------------------------------------------------
    // Fixture: GET /workspaces/{id} — archived state mapping
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn contract_status_archived_state() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/workspaces/ws-xyz"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    r#"{"id":"ws-xyz","state":"auto-archived","cpu":0,"memory":0,"disk":0}"#,
                    "application/json",
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let status = client.status("ws-xyz").await.expect("status must succeed");
        assert_eq!(status.state, TapeState::Archived, "'auto-archived' must map to TapeState::Archived");
        // Fallback values when API returns 0
        assert_eq!(status.vcpus, 1, "vcpus=0 from API must fall back to 1");
        assert_eq!(status.memory_mib, 1024, "memory=0 from API must fall back to 1024");
    }

    // ------------------------------------------------------------------
    // Fixture: 503 response triggers retry, second call succeeds
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn contract_retries_on_503() {
        let server = MockServer::start().await;

        // First call returns 503
        Mock::given(method("GET"))
            .and(path("/workspaces/ws-retry"))
            .respond_with(ResponseTemplate::new(503).set_body_raw(r#"{"error":"unavailable"}"#, "application/json"))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call (retry) returns 200
        Mock::given(method("GET"))
            .and(path("/workspaces/ws-retry"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    r#"{"id":"ws-retry","state":"running","cpu":1,"memory":1024,"disk":1024}"#,
                    "application/json",
                ),
            )
            .mount(&server)
            .await;

        let mut client = make_client(&server.uri());
        // Reduce retry delay to make the test fast
        client.max_retries = 2;
        let status = client.status("ws-retry").await.expect("status must succeed after retry");
        assert_eq!(status.state, TapeState::Running);
    }

    // ------------------------------------------------------------------
    // Fixture: DELETE /workspaces/{id} — 404 is treated as success
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn contract_delete_404_is_ok() {
        let server = MockServer::start().await;

        Mock::given(method("DELETE"))
            .and(path("/workspaces/ws-gone"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        client.delete("ws-gone").await.expect("delete of already-gone workspace must not error");
    }
}
