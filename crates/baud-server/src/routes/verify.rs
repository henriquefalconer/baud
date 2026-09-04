// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /verify — determinism and observation verification routes (M3)
//
// Routes:
//   POST /verify/determinism   → run spec twice, compare observation stream hashes
//   GET  /verify/observation   → (stub, M7) cross-check syscall log vs eBPF

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;
use baud_proto::{Observation, Value as ProbeValue};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct VerifyDeterminismBody {
    /// Raw spec content (YAML)
    pub spec: String,
    /// RNG seed
    #[serde(default)]
    pub seed: u64,
    /// Number of times to run (default 2, minimum 2)
    #[serde(default = "default_times")]
    pub times: u32,
}

fn default_times() -> u32 { 2 }

// ---------------------------------------------------------------------------
// POST /verify/determinism
// ---------------------------------------------------------------------------

pub async fn determinism(
    State(state): State<AppState>,
    Json(body): Json<VerifyDeterminismBody>,
) -> Json<Value> {
    // 1. Lint the spec
    let spec_doc = match baud_init::lint(&body.spec) {
        Ok(doc) => doc,
        Err(e) => return Json(json!({ "ok": false, "error": format!("spec lint error: {e}") })),
    };

    let times = body.times.max(2);
    let seed = body.seed;

    // 2. Run the spec `times` times and collect observation stream hashes
    let mut run_hashes: Vec<String> = Vec::new();
    let mut run_ids: Vec<String> = Vec::new();
    let mut first_obs_count = 0usize;

    for i in 0..times {
        // Create a run record for each execution
        let run_id = format!("verify-{}-{i}", uuid::Uuid::new_v4().to_string().replace('-', "").chars().take(8).collect::<String>());
        let now = crate::state::unix_now() as i64;
        let spec_hash = format!("blake3:{}", hex_encode(blake3::hash(body.spec.as_bytes()).as_bytes()));
        let closure_hash = format!("blake3:{}", hex_encode(blake3::hash(
            format!("{}:{}", spec_doc.nix, spec_doc.nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>().join(",")).as_bytes()
        ).as_bytes()));

        let _ = sqlx::query(
            "INSERT INTO runs (id, spec_content, spec_hash, nix_ref, closure_hash, strategy, tactics, seed, budget_minutes, tape_id, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, 5, NULL, 'done', ?, ?)"
        )
        .bind(&run_id)
        .bind(&body.spec)
        .bind(&spec_hash)
        .bind(&spec_doc.nix)
        .bind(&closure_hash)
        .bind(seed as i64)
        .bind(now)
        .bind(now)
        .execute(&state.db)
        .await;

        // 3. Execute the workload and collect observations. A load or execution failure is a
        // failed verification, never an empty stream that can accidentally look deterministic.
        let observations = match run_spec_through_multiverse(seed, &spec_hash, &spec_doc) {
            Ok(observations) => observations,
            Err(error) => {
                let _ = sqlx::query("UPDATE runs SET status = 'error', updated_at = ? WHERE id = ?")
                    .bind(crate::state::unix_now() as i64)
                    .bind(&run_id)
                    .execute(&state.db)
                    .await;
                return Json(json!({
                    "ok": false,
                    "verified": false,
                    "run_id": run_id,
                    "error": error,
                    "message": "determinism verification failed before an observation stream was produced"
                }));
            }
        };

        // 4. Append observations to the run (in-process: write to SQLite + in-memory journal)
        let mut journal_hasher = blake3::Hasher::new();
        for obs in &observations {
            let now = crate::state::unix_now() as i64;
            // Persist the typed protocol value, not its Debug rendering, so a bounded replay can
            // reconstruct the exact original observation hash later.
            let value_bytes = serde_json::to_vec(&obs.value).unwrap_or_default();

            // Feed into stream hash (over the serialized observation)
            let obs_cbor = baud_proto::encode(&baud_proto::Msg::Observe(obs.clone()))
                .unwrap_or_default();
            journal_hasher.update(&obs_cbor);

            let _ = sqlx::query(
                "INSERT INTO observations (run_id, step, node, probe, value, recorded_at)
                 VALUES (?, ?, ?, ?, ?, ?)"
            )
            .bind(&run_id)
            .bind(obs.step as i64)
            .bind(obs.node as i64)
            .bind(&obs.probe)
            .bind(&value_bytes)
            .bind(now)
            .execute(&state.db)
            .await;
        }

        if i == 0 { first_obs_count = observations.len(); }

        let stream_hash = hex_encode(journal_hasher.finalize().as_bytes());

        // Store the stream_hash in the runs table for later replay verification
        let _ = sqlx::query("UPDATE runs SET stream_hash = ? WHERE id = ?")
            .bind(&stream_hash)
            .bind(&run_id)
            .execute(&state.db)
            .await;

        run_hashes.push(stream_hash);
        run_ids.push(run_id);
    }

    // 5. Compare all hashes
    let all_match = run_hashes.windows(2).all(|w| w[0] == w[1]);

    // Guard against false-positives on empty streams: if neither run produced
    // any observations, we cannot claim determinism — the supervisor failed to run.
    if all_match && first_obs_count == 0 {
        return Json(json!({
            "ok": false,
            "verified": false,
            "times": times,
            "seed": seed,
            "stream_hashes": run_hashes,
            "run_ids": run_ids,
            "observation_count": 0,
            "error": "multiverse produced no observations — load or execution may have failed",
            "message": "DETERMINISM UNVERIFIED: no observations produced (cannot verify empty streams)",
        }));
    }

    if all_match {
        Json(json!({
            "ok": true,
            "verified": true,
            "times": times,
            "seed": seed,
            "stream_hashes": run_hashes,
            "run_ids": run_ids,
            "observation_count": first_obs_count,
            "message": "determinism verified: all runs produced identical observation stream hashes",
        }))
    } else {
        // Find first divergence
        let first_divergence = find_first_divergence(&run_hashes);
        // Mark all runs from this divergent verification as 'divergent' so they
        // are excluded from replay/shrink/reconstruct (spec baud-journal §5 / VR2-M15).
        for rid in &run_ids {
            let _ = sqlx::query("UPDATE runs SET status = 'divergent', updated_at = ? WHERE id = ?")
                .bind(crate::state::unix_now() as i64)
                .bind(rid)
                .execute(&state.db)
                .await;
        }
        Json(json!({
            "ok": false,
            "verified": false,
            "times": times,
            "seed": seed,
            "stream_hashes": run_hashes,
            "run_ids": run_ids,
            "first_divergence": first_divergence,
            "message": "DETERMINISM VIOLATION: runs produced different observation stream hashes",
        }))
    }
}

/// Simulate a poisoned run that is NOT deterministic (injects current time as noise).
/// Used by the drive script to demonstrate that the verifier catches non-determinism.
pub async fn determinism_poisoned(
    State(state): State<AppState>,
    Json(body): Json<VerifyDeterminismBody>,
) -> Json<Value> {
    // Same as determinism but adds time-based noise to break determinism
    let spec_doc = match baud_init::lint(&body.spec) {
        Ok(doc) => doc,
        Err(e) => return Json(json!({ "ok": false, "error": format!("spec lint error: {e}") })),
    };

    let times = body.times.max(2);
    let seed = body.seed;
    let spec_hash = format!("blake3:{}", hex_encode(blake3::hash(body.spec.as_bytes()).as_bytes()));
    let mut run_hashes: Vec<String> = Vec::new();
    let mut run_ids: Vec<String> = Vec::new();
    // Collect all observation streams for step-by-step comparison
    let mut all_run_observations: Vec<Vec<Observation>> = Vec::new();

    for i in 0..times {
        let run_id = format!("poisoned-{}-{i}", uuid::Uuid::new_v4().to_string().replace('-', "").chars().take(8).collect::<String>());
        let now = crate::state::unix_now() as i64;
        let closure_hash = format!("blake3:{}", hex_encode(blake3::hash(spec_doc.nix.as_bytes()).as_bytes()));

        let _ = sqlx::query(
            "INSERT INTO runs (id, spec_content, spec_hash, nix_ref, closure_hash, seed, budget_minutes, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 5, 'done', ?, ?)"
        )
        .bind(&run_id)
        .bind(&body.spec)
        .bind(&spec_hash)
        .bind(&spec_doc.nix)
        .bind(&closure_hash)
        .bind(seed as i64)
        .bind(now)
        .bind(now)
        .execute(&state.db)
        .await;

        let mut journal_hasher = blake3::Hasher::new();
        let base_obs = match run_spec_through_multiverse(seed, &spec_hash, &spec_doc) {
            Ok(observations) => observations,
            Err(error) => {
                let _ = sqlx::query("UPDATE runs SET status = 'error', updated_at = ? WHERE id = ?")
                    .bind(crate::state::unix_now() as i64)
                    .bind(&run_id)
                    .execute(&state.db)
                    .await;
                return Json(json!({
                    "ok": false,
                    "verified": false,
                    "run_id": run_id,
                    "error": error,
                    "message": "poisoned verification failed before an observation stream was produced"
                }));
            }
        };

        // Inject time-based poison: different for each run (appended after base observations)
        let poison = crate::state::unix_now();
        let poison_obs = Observation {
            probe: "time_poison".into(),
            node: 0,
            value: ProbeValue::U64(poison + i as u64),
            step: base_obs.len() as u64, // Poison appears at index == base_obs.len()
        };

        let mut full_obs = base_obs;
        full_obs.push(poison_obs);

        for obs in &full_obs {
            let obs_cbor = baud_proto::encode(&baud_proto::Msg::Observe(obs.clone()))
                .unwrap_or_default();
            journal_hasher.update(&obs_cbor);
        }

        let stream_hash = hex_encode(journal_hasher.finalize().as_bytes());
        run_hashes.push(stream_hash);
        run_ids.push(run_id);
        all_run_observations.push(full_obs);
    }

    let all_match = run_hashes.windows(2).all(|w| w[0] == w[1]);

    // Find first divergent step by comparing observation sequences pairwise
    struct DivergenceInfo {
        step: u64,
        node: Option<u16>,
        probe: Option<String>,
        detail: String,
    }

    let divergence: Option<DivergenceInfo> = if !all_match && all_run_observations.len() >= 2 {
        let run0 = &all_run_observations[0];
        let run1 = &all_run_observations[1];
        let min_len = run0.len().min(run1.len());
        let mut found: Option<DivergenceInfo> = None;
        for idx in 0..min_len {
            let o0_cbor = baud_proto::encode(&baud_proto::Msg::Observe(run0[idx].clone())).unwrap_or_default();
            let o1_cbor = baud_proto::encode(&baud_proto::Msg::Observe(run1[idx].clone())).unwrap_or_default();
            if o0_cbor != o1_cbor {
                found = Some(DivergenceInfo {
                    step: run0[idx].step,
                    node: Some(run0[idx].node),
                    probe: Some(run0[idx].probe.clone()),
                    detail: format!(
                        "probe '{}' on node {} diverged at step {}",
                        run0[idx].probe, run0[idx].node, run0[idx].step
                    ),
                });
                break;
            }
        }
        // If all shared steps match but lengths differ, the divergence is at the next step
        if found.is_none() && run0.len() != run1.len() {
            let shorter = if run0.len() < run1.len() { run0 } else { run1 };
            let step = shorter.last().map(|o| o.step + 1).unwrap_or(0);
            found = Some(DivergenceInfo {
                step,
                node: None,
                probe: None,
                detail: format!("runs produced different observation counts ({} vs {})", run0.len(), run1.len()),
            });
        }
        found
    } else {
        None
    };

    Json(json!({
        "ok": false,
        "verified": false,
        "poisoned": true,
        "times": times,
        "seed": seed,
        "stream_hashes": run_hashes,
        "run_ids": run_ids,
        "first_divergent_step": divergence.as_ref().map(|d| d.step),
        "divergent_node": divergence.as_ref().and_then(|d| d.node),
        "divergent_probe": divergence.as_ref().and_then(|d| d.probe.as_deref()),
        "divergent_detail": divergence.as_ref().map(|d| d.detail.as_str()),
        "message": if all_match {
            "WARNING: poisoned run did not diverge (unexpected)"
        } else {
            "DETERMINISM VIOLATION detected (expected for poisoned run)"
        },
    }))
}

/// GET /verify/observation/:run_id — cross-check syscall log vs eBPF (M7)
///
/// Fetches plane-1 (syscall_records) and plane-2 (ebpf_records) for the run,
/// runs baud_tracing::cross_check, stores the result, and returns it.
pub async fn observation(
    State(state): State<AppState>,
    axum::extract::Path(run_id): axum::extract::Path<String>,
) -> Json<Value> {
    // Ensure run exists
    let run_exists = sqlx::query_as::<_, (String,)>("SELECT id FROM runs WHERE id = ?")
        .bind(&run_id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None)
        .is_some();

    if !run_exists {
        return Json(json!({ "ok": false, "error": format!("run not found: {run_id}") }));
    }

    // Fetch plane-1: syscall records from supervisor (untyped)
    let syscall_rows: Vec<(i64, i64, Vec<u8>, i64, i64)> = sqlx::query_as(
        "SELECT node, sysno, args_digest, ret, vtime FROM syscall_records
         WHERE run_id = ? ORDER BY vtime ASC"
    )
    .bind(&run_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let syscall_records: Vec<baud_proto::SyscallRecord> = syscall_rows.iter().map(|(node, sysno, args_digest, ret, vtime)| {
        let mut digest = [0u8; 32];
        let b = args_digest;
        let len = b.len().min(32);
        digest[..len].copy_from_slice(&b[..len]);
        baud_proto::SyscallRecord {
            node: *node as u16,
            sysno: *sysno as u32,
            args_digest: baud_proto::Hash(digest),
            ret: *ret,
            vtime: *vtime as u64,
        }
    }).collect();

    // Fetch plane-2: eBPF records (untyped)
    let ebpf_rows: Vec<(i64, String, i64, i64, String)> = sqlx::query_as(
        "SELECT node, event, value, vtime, source FROM ebpf_records
         WHERE run_id = ? ORDER BY vtime ASC"
    )
    .bind(&run_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    // Build a TracingSession from the stored eBPF records (now tuples: node, event, value, vtime, source)
    let mut session = baud_tracing::TracingSession::new(&run_id);
    // Register pids (synthetic: pid = 1000 + node)
    for (node, _event, _value, _vtime, _source) in &ebpf_rows {
        let node_u16 = *node as u16;
        let pid = 1000 + node_u16 as u32;
        session.register_pid(pid, node_u16);
    }
    // Replay eBPF records into the session to populate syscall counts
    for (node, event, _value, vtime, _source) in &ebpf_rows {
        if event.starts_with("syscall:") {
            let node_u16 = *node as u16;
            let pid = 1000 + node_u16 as u32;
            let sysno: u32 = event.trim_start_matches("syscall:").parse().unwrap_or(0);
            session.ingest_syscall(pid, sysno, *vtime as u64);
        }
    }

    // Run the cross-check
    let result = baud_tracing::cross_check(&run_id, &syscall_records, &session);

    // Store the result (untyped)
    let now = crate::state::unix_now() as i64;
    let passed_i = if result.passed { 1i64 } else { 0i64 };
    let div_node = result.divergent_node.map(|n| n as i64);
    let p2_source_str = if matches!(result.plane2_source, baud_proto::Source::Native) { "native" } else { "fallback" };
    let _ = sqlx::query(
        "INSERT INTO observation_checks (run_id, passed, divergent_node, plane2_source, message, checked_at)
         VALUES (?, ?, ?, ?, ?, ?)"
    )
    .bind(&run_id)
    .bind(passed_i)
    .bind(div_node)
    .bind(p2_source_str)
    .bind(&result.message)
    .bind(now)
    .execute(&state.db)
    .await;

    let plane1_map: serde_json::Map<String, Value> = result.plane1_counts.iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();
    let plane2_map: serde_json::Map<String, Value> = result.plane2_counts.iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();

    let exit_code = if result.passed { 0 } else { 1 };

    Json(json!({
        "ok": result.passed,
        "run_id": run_id,
        "passed": result.passed,
        "divergent_node": result.divergent_node,
        "plane1_counts": plane1_map,
        "plane2_counts": plane2_map,
        "plane2_source": if matches!(result.plane2_source, baud_proto::Source::Native) { "native" } else { "fallback" },
        "message": result.message,
        "exit_code": exit_code,
        "syscall_records_total": syscall_records.len(),
        "ebpf_records_total": ebpf_rows.len(),
    }))
}

// ---------------------------------------------------------------------------
// Real deterministic execution via baud-multiverse
// ---------------------------------------------------------------------------

/// Run the workload spec through baud-multiverse once and return the observation stream.
///
/// Uses a seeded tape (`TapeDrawSource`) generated from the seed. Because
/// baud-multiverse's run is a pure function of (binary, manifest, tape),
/// two calls with the same (seed, spec) must produce byte-identical observation
/// stream hashes — that is what `verify/determinism` checks.
fn run_spec_through_multiverse(
    seed: u64,
    _spec_hash: &str,
    spec_doc: &baud_init::parse::SpecDoc,
) -> Result<Vec<Observation>, String> {
    use baud_multiverse::{Multiverse, RunManifest, GuestSpec, TapeDrawSource};

    // Build the run manifest from the spec doc.
    let manifest = RunManifest {
        guests: spec_doc.nodes.iter().enumerate().map(|(i, n)| GuestSpec {
            node_id: i as u32,
            binary: std::path::PathBuf::from(&n.argv.first().cloned().unwrap_or_default()),
            argv: n.argv.clone(),
        }).collect(),
        env_override: spec_doc.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        ..RunManifest::default()
    };

    // Generate a deterministic tape from the seed (using seed bytes as tape content).
    // In full implementation, this is the tape from baud-driver.
    let tape_bytes = generate_tape_from_seed(seed, 4096);
    let mut tape_source = TapeDrawSource::new(tape_bytes);

    // Load and run the multiverse.
    let mut mv = match Multiverse::load_from_manifest(manifest) {
        Ok(mv) => mv,
        Err(e) => return Err(format!("multiverse failed to load the workload: {e}")),
    };

    // run() is now infallible (returns ObservationStream directly, spec §5)
    let stream = mv.run(&mut tape_source);
    Ok(stream.observations.iter().map(|e| Observation {
        probe: e.probe.clone(),
        node: e.node as u16,
        value: ProbeValue::Utf8(e.value.to_string()),
        step: e.step,
    }).collect())
}

/// Generate a deterministic tape from a seed (using ChaCha PRNG).
fn generate_tape_from_seed(seed: u64, len: usize) -> Vec<u8> {
    use rand_chacha::ChaCha20Rng;
    use rand::{RngCore, SeedableRng};

    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut tape = vec![0u8; len];
    rng.fill_bytes(&mut tape);
    tape
}

fn find_first_divergence(hashes: &[String]) -> Option<usize> {
    for (i, w) in hashes.windows(2).enumerate() {
        if w[0] != w[1] {
            return Some(i + 1);
        }
    }
    None
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
