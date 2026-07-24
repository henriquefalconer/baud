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

    pub async fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let resp = self.http.get(&url).send().await
            .with_context(|| format!("GET {url}: could not connect to baud-server"))?;
        let status = resp.status();
        let body: Value = resp.json().await
            .with_context(|| format!("GET {url}: invalid JSON response"))?;
        if !status.is_success() {
            anyhow::bail!("GET {url} returned {status}: {body}");
        }
        Ok(body)
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let resp = self.http.delete(&url).send().await
            .with_context(|| format!("DELETE {url}: could not connect to baud-server"))?;
        let status = resp.status();
        let body: Value = resp.json().await
            .with_context(|| format!("DELETE {url}: invalid JSON response"))?;
        if !status.is_success() {
            anyhow::bail!("DELETE {url} returned {status}: {body}");
        }
        Ok(body)
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let resp = self.http.post(&url).json(body).send().await
            .with_context(|| format!("POST {url}: could not connect to baud-server"))?;
        let status = resp.status();
        let resp_body: Value = resp.json().await
            .with_context(|| format!("POST {url}: invalid JSON response"))?;
        if !status.is_success() {
            anyhow::bail!("POST {url} returned {status}: {resp_body}");
        }
        Ok(resp_body)
    }
}
