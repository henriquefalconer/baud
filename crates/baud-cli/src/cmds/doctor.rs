// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use anyhow::Result;
use crate::{client::Client, fmt};

pub async fn run(c: &Client, json: bool) -> Result<()> {
    let v = c.get("/doctor").await?;
    fmt::print(&v, json);
    // Exit 1 if any check failed
    let all_ok = v.get("sops").and_then(|s| s.get("ok")).and_then(|v| v.as_bool()).unwrap_or(false)
        && v.get("age").and_then(|s| s.get("ok")).and_then(|v| v.as_bool()).unwrap_or(false);
    if !all_ok {
        std::process::exit(1);
    }
    Ok(())
}
