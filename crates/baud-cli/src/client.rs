// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[error("HTTP {status}: {body}")]
pub struct ClientError {
    pub status: u16,
    pub body: Value,
}

/// Thin HTTP client for the baud-server REST API.
pub struct Client {
    base: String,
    http: reqwest::Client,
}

pub(crate) fn auth_token() -> Result<Option<String>> {
    if let Some(path) = std::env::var_os("BAUD_AUTH_TOKEN_FILE") {
        let token = std::fs::read_to_string(path).context("failed to read BAUD_AUTH_TOKEN_FILE")?;
        let token = token.trim_end_matches(['\r', '\n']).to_owned();
        if token.is_empty() || token.chars().any(char::is_whitespace) {
            anyhow::bail!("BAUD_AUTH_TOKEN_FILE contains an empty or whitespace-bearing token");
        }
        return Ok(Some(token));
    }
    if let Some(token) = std::env::var("BAUD_AUTH_TOKEN").ok() {
        if token.is_empty() || token.chars().any(char::is_whitespace) {
            anyhow::bail!("BAUD_AUTH_TOKEN contains an empty or whitespace-bearing token");
        }
        return Ok(Some(token));
    }

    // A server configured with BAUD_IDENTITY_SEED_B64 accepts signed agent tokens as well as
    // the local bearer token. Mint one here so the CLI does not silently become unauthenticated
    // when an operator uses the identity configuration. The seed is read only from the process
    // environment and the resulting JWT is kept as a String only for reqwest's header API.
    let Some(seed) = std::env::var("BAUD_IDENTITY_SEED_B64").ok() else {
        return Ok(None);
    };
    let root = baud_identity::RootKey::from_seed_b64(&seed)
        .context("BAUD_IDENTITY_SEED_B64 is not a valid identity seed")?;
    let token = root
        .mint_tape_token("cli", "client")
        .context("failed to mint CLI identity token")?;
    Ok(Some(token.expose().to_owned()))
}

async fn response_json(response: reqwest::Response, url: &str, method: &str) -> Result<Value> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("{method} {url}: failed to read response"))?;
    if bytes.is_empty() {
        return Ok(serde_json::json!({
            "ok": false,
            "error": format!("{method} {url} returned an empty response"),
            "http_status": status.as_u16(),
        }));
    }
    match serde_json::from_slice(&bytes) {
        Ok(body) => Ok(body),
        Err(_) => Ok(serde_json::json!({
            "ok": false,
            "error": String::from_utf8_lossy(&bytes).trim().to_owned(),
            "http_status": status.as_u16(),
        })),
    }
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
        if let Some(token) = auth_token()? {
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .await
            .with_context(|| format!("GET {url}: could not connect to baud-server"))?;
        let status = resp.status();
        let body = response_json(resp, &url, "GET").await?;
        if !status.is_success() {
            return Err(ClientError {
                status: status.as_u16(),
                body,
            }
            .into());
        }
        Ok(body)
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.http.delete(&url);
        if let Some(token) = auth_token()? {
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .await
            .with_context(|| format!("DELETE {url}: could not connect to baud-server"))?;
        let status = resp.status();
        let body = response_json(resp, &url, "DELETE").await?;
        if !status.is_success() {
            return Err(ClientError {
                status: status.as_u16(),
                body,
            }
            .into());
        }
        Ok(body)
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.http.post(&url).json(body);
        if let Some(token) = auth_token()? {
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .await
            .with_context(|| format!("POST {url}: could not connect to baud-server"))?;
        let status = resp.status();
        let resp_body = response_json(resp, &url, "POST").await?;
        if !status.is_success() {
            return Err(ClientError {
                status: status.as_u16(),
                body: resp_body,
            }
            .into());
        }
        Ok(resp_body)
    }

    /// Copy an SSE response to stdout until the server or caller closes it. Keeping this in the
    /// client makes `obs tail` and frame tailing genuine streaming commands instead of silently
    /// degrading to a one-shot JSON snapshot.
    pub async fn stream_get(&self, path: &str) -> Result<()> {
        self.stream_get_to(path, None).await
    }

    /// Consume an SSE response either on stdout or into a caller-selected file. The file is
    /// created before the request starts, so a successful command always leaves a complete byte
    /// stream and a failed request never silently discards the requested output destination.
    pub async fn stream_get_to(&self, path: &str, output: Option<&std::path::Path>) -> Result<()> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.http.get(&url).header("accept", "text/event-stream");
        if let Some(token) = auth_token()? {
            request = request.bearer_auth(token);
        }
        let mut sink: Box<dyn std::io::Write> = match output {
            Some(path) => Box::new(
                std::fs::File::create(path)
                    .with_context(|| format!("create stream output {}", path.display()))?,
            ),
            None => Box::new(std::io::stdout()),
        };
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
            sink.write_all(&chunk)?;
            sink.flush()?;
        }
        Ok(())
    }
}
