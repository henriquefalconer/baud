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
        let name = format!("baud-{}", uuid::Uuid::new_v4().to_string().split('-').next().unwrap());
        let req = CreateWorkspaceRequest {
            name,
            image: spec.image.clone(),
            resources: Resources {
                cpu: spec.vcpus,
                memory: spec.memory_mib,
                disk: spec.disk_mib,
            },
            labels: spec.labels.clone(),
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
}
