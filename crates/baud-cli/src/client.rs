// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use anyhow::{Context, Result};
use serde_json::Value;

/// Thin HTTP client for the baud-server REST API.
pub struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base: &str) -> Self {
        Client {
            base: base.trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }

    /// `path`'s full `ws://`/`wss://` URL against this client's configured server — `--server`'s
    /// `http`/`https` scheme swapped for the WebSocket equivalent, everything else (host, port)
    /// unchanged. Used by `baud shell-into` (`cmds/shell_into.rs`), the first command in this
    /// crate to speak WebSocket instead of one-shot REST.
    pub fn ws_url(&self, path: &str) -> String {
        let ws_base = if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.base.clone()
        };
        format!("{ws_base}{path}")
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.http.get(&url);
        if let Ok(token) = std::env::var("BAUD_AUTH_TOKEN") {
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .await
            .with_context(|| format!("GET {url}: could not connect to baud-server"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .with_context(|| format!("GET {url}: invalid JSON response"))?;
        if !status.is_success() {
            anyhow::bail!("GET {url} returned {status}: {body}");
        }
        Ok(body)
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.http.delete(&url);
        if let Ok(token) = std::env::var("BAUD_AUTH_TOKEN") {
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .await
            .with_context(|| format!("DELETE {url}: could not connect to baud-server"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .with_context(|| format!("DELETE {url}: invalid JSON response"))?;
        if !status.is_success() {
            anyhow::bail!("DELETE {url} returned {status}: {body}");
        }
        Ok(body)
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.http.post(&url).json(body);
        if let Ok(token) = std::env::var("BAUD_AUTH_TOKEN") {
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .await
            .with_context(|| format!("POST {url}: could not connect to baud-server"))?;
        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .with_context(|| format!("POST {url}: invalid JSON response"))?;
        if !status.is_success() {
            anyhow::bail!("POST {url} returned {status}: {resp_body}");
        }
        Ok(resp_body)
    }

    /// Copy an SSE response to stdout until the server or caller closes it. Keeping this in the
    /// client makes `obs tail` and frame tailing genuine streaming commands instead of silently
    /// degrading to a one-shot JSON snapshot.
    pub async fn stream_get(&self, path: &str) -> Result<()> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.http.get(&url).header("accept", "text/event-stream");
        if let Ok(token) = std::env::var("BAUD_AUTH_TOKEN") {
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .await
            .with_context(|| format!("GET {url}: could not connect to baud-server"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET {url} returned {status}: {body}");
        }
        let mut resp = resp;
        while let Some(chunk) = resp
            .chunk()
            .await
            .with_context(|| format!("GET {url}: stream read failed"))?
        {
            use std::io::Write;
            std::io::stdout().write_all(&chunk)?;
            std::io::stdout().flush()?;
        }
        Ok(())
    }
}
