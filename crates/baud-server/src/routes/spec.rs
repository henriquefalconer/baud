// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /spec — spec operations (M2)
//
// Routes:
//   POST /spec/lint → lint a YAML spec (spec lint)
//   POST /spec/show → parse and show a spec (spec show)

use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct SpecBody {
    /// Raw YAML spec content
    pub content: String,
}

/// POST /spec/lint — lint a YAML spec
pub async fn lint(Json(body): Json<SpecBody>) -> Json<Value> {
    match baud_init::lint(&body.content) {
        Ok(doc) => {
            let nodes: Vec<Value> = doc
                .nodes
                .iter()
                .map(|n| json!({ "name": n.name, "argv": n.argv }))
                .collect();
            Json(json!({
                "ok": true,
                "nix": doc.nix,
                "nodes": nodes,
                "env_keys": doc.env.keys().collect::<Vec<_>>(),
                "files_count": doc.files.len(),
            }))
        }
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// POST /spec/show — parse and return the full spec
pub async fn show(Json(body): Json<SpecBody>) -> Json<Value> {
    match baud_init::lint(&body.content) {
        Ok(doc) => Json(serde_json::to_value(&doc).unwrap_or_else(|e| {
            json!({ "error": format!("serialization error: {e}") })
        })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
