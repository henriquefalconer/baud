// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// GET /host/probe — `baud host probe` (specs/baud-host.md §3, milestone H0)
//
// baud-server runs on the host itself, so probing here probes the real machine baud-multiverse
// would run guests on. A failing capability is reported, never hidden — every field is a raw,
// independently-observed check; `runnable`/`enforced_capable` are derived from those checks
// (`Probe::is_runnable`/`is_enforced_capable`), not a separate summary tier.

use axum::{extract::State, Json};
use baud_host::Host;
use serde_json::{json, Value};
use crate::AppState;

pub async fn probe(_state: State<AppState>) -> Json<Value> {
    // Capability probing does real ioctls/syscalls; run it off the async executor.
    let host = tokio::task::spawn_blocking(Host::probe)
        .await
        .expect("host probe task panicked");

    Json(json!({
        "kvm": host.kvm,
        "vmx": host.vmx,
        "cpuid": host.cpuid,
        "tsc_stable": host.tsc_stable,
        "msr_filter": host.msr_filter,
        "singlestep": host.singlestep,
        "rcb_deterministic": host.rcb_deterministic,
        "nested": host.nested,
        "vendor": host.vendor,
        "enforced_module_present": host.enforced_module_present,
        "runnable": host.is_runnable(),
        "enforced_capable": host.is_enforced_capable(),
        "reason": host.reason,
        "capacity": host.capacity(),
    }))
}
