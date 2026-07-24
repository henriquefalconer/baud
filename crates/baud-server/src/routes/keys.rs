// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;

#[derive(Deserialize)]
pub struct KeysInitBody {
    pub age_recipient: String,
    /// Path to a template plaintext YAML file (defaults to infra/secrets/baud.enc.yaml.example)
    #[serde(default)]
    pub template_path: Option<String>,
    /// Output path for the encrypted file (defaults to infra/secrets/baud.enc.yaml)
    #[serde(default)]
    pub out_path: Option<String>,
}

/// POST /keys/init — `baud keys init`
pub async fn init(
    _state: State<AppState>,
    Json(body): Json<KeysInitBody>,
) -> Json<Value> {
    let template = body.template_path
        .as_deref()
        .unwrap_or("infra/secrets/baud.enc.yaml.example");
    let out = body.out_path
        .as_deref()
        .unwrap_or("infra/secrets/baud.enc.yaml");

    match baud_keys::init_secrets(
        &body.age_recipient,
        std::path::Path::new(template),
        std::path::Path::new(out),
    ) {
        Ok(()) => Json(json!({ "ok": true, "out_path": out })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// GET /keys/show — `baud keys show --redacted`
/// Returns all key names with values replaced by "[REDACTED]".
pub async fn show(_state: State<AppState>) -> Json<Value> {
    let secrets_path = baud_keys::secrets_file();
    match baud_keys::show_redacted(&secrets_path) {
        Ok(map) => {
            // Convert map to a stable JSON object
            let obj: serde_json::Map<String, Value> = map
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect();
            Json(Value::Object(obj))
        }
        Err(e) => Json(json!({
            "ok": false,
            "error": e.to_string(),
            "note": "could not decrypt secrets file; check sops/age installation and key path"
        })),
    }
}

/// POST /keys/rotate — `baud keys rotate`
/// Rotates the sops data encryption key (does not change recipients or age identity).
pub async fn rotate(_state: State<AppState>) -> Json<Value> {
    let secrets_path = baud_keys::secrets_file();
    match baud_keys::rotate_secrets(&secrets_path) {
        Ok(()) => Json(json!({ "ok": true, "message": "sops data key rotated successfully" })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}
