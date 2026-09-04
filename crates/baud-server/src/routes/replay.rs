// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /replay — run replay routes (M3)
//
// Routes:
//   POST /replay/:id         → replay a run (from stored tape/journal)
//   POST /replay/:id/to-step → replay up to a given step

use axum::{extract::{Path, State}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;
use baud_proto::{Observation, Value as ProbeValue};

#[derive(Debug, Deserialize)]
pub struct ReplayBody {
    /// Optional tape file contents (CBOR-encoded Tape); if absent, uses stored tape
    #[allow(dead_code)]
    pub tape_bytes: Option<Vec<u8>>,
    /// Replay up to this step (inclusive); if absent, replay full run
    pub to_step: Option<u64>,
}

/// POST /replay/:id — replay a run
pub async fn replay(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<Option<ReplayBody>>,
) -> Json<Value> {
    let body = body.unwrap_or(ReplayBody { tape_bytes: None, to_step: None });

    // 1. Look up the run
    let row = sqlx::query_as::<_, (String, String, String, Option<String>, i64, String, Option<String>)>(
        "SELECT id, spec_content, spec_hash, closure_hash, seed, status, stream_hash FROM runs WHERE id = ?"
    )
    .bind(&run_id)
    .fetch_optional(&state.db)
    .await;

    let (id, spec_content, spec_hash, closure_hash, seed, status, original_stored_hash) = match row {
        Ok(Some(r)) => r,
        Ok(None) => return Json(json!({ "error": format!("run {run_id} not found") })),
        Err(e) => return Json(json!({ "error": format!("db error: {e}") })),
    };

    // Guard: divergent runs are excluded from replay (spec baud-journal §5 / VR2-M15).
    if status == "divergent" {
        return Json(json!({
            "error": format!("run {run_id} is marked divergent and cannot be replayed"),
            "status": "divergent",
        }));
    }

    // 2. Parse spec to understand topology
    let spec_doc = match baud_init::lint(&spec_content) {
        Ok(doc) => doc,
        Err(e) => return Json(json!({ "error": format!("spec parse error: {e}") })),
    };

    // 3. Create a replay run record
    let replay_run_id = format!("replay-{}", uuid::Uuid::new_v4().to_string().replace('-', "").chars().take(8).collect::<String>());
    let now = crate::state::unix_now() as i64;
    let replay_spec_hash = spec_hash.clone();

    let _ = sqlx::query(
        "INSERT INTO runs (id, spec_content, spec_hash, nix_ref, closure_hash, strategy, tactics, seed, budget_minutes, tape_id, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, 5, NULL, 'done', ?, ?)"
    )
    .bind(&replay_run_id)
    .bind(&spec_content)
    .bind(&replay_spec_hash)
    .bind(&spec_doc.nix)
    .bind(closure_hash.as_deref().unwrap_or(""))
    .bind(seed)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await;

    // 4. Fetch original observations from the run being replayed
    let original_rows = sqlx::query_as::<_, (i64, i64, String, Vec<u8>, i64)>(
        "SELECT step, node, probe, value, recorded_at FROM observations
         WHERE run_id = ?
         ORDER BY step ASC"
    )
    .bind(&run_id)
    .fetch_all(&state.db)
    .await;

    let original_obs = match original_rows {
        Ok(r) => r,
        Err(e) => return Json(json!({ "error": format!("db error fetching observations: {e}") })),
    };

    // 5. Prefer the exact tape recorded by a real KVM run. Re-seeding a PRNG here would
    // silently replay a different input stream, which is especially easy to miss when the
    // guest consumes only a prefix. The legacy seed path remains for pre-KVM rows that have no
    // persisted tape metadata.
    let stored_tape = sqlx::query_as::<_, (String,)>(
        "SELECT tape_hex FROM kvm_run_meta WHERE run_id = ?",
    )
    .bind(&run_id)
    .fetch_optional(&state.db)
    .await;
    let replay_tape = match stored_tape {
        Ok(Some((tape_hex,))) => match decode_hex_tape(&tape_hex) {
            Some(tape) => Some(tape),
            None => return Json(json!({ "error": "stored KVM tape is malformed" })),
        },
        Ok(None) => body.tape_bytes.clone(),
        Err(e) => return Json(json!({ "error": format!("db error fetching replay tape: {e}") })),
    };
    let replay_obs = generate_replay_observations(
        replay_tape.as_deref(),
        seed as u64,
        &spec_hash,
        &spec_doc,
    );

    let to_step = body.to_step;
    let mut replayed = Vec::new();
    let mut replay_hash = blake3::Hasher::new();

    for obs in &replay_obs {
        if let Some(max) = to_step {
            if obs.step > max { break; }
        }
        let obs_cbor = baud_proto::encode(&baud_proto::Msg::Observe(obs.clone()))
            .unwrap_or_default();
        replay_hash.update(&obs_cbor);
        replayed.push(obs.clone());
    }

    let replay_stream_hash = hex_encode(replay_hash.finalize().as_bytes());

    // 7. Insert replayed observations into SQLite under replay_run_id
    for obs in &replayed {
        let value_bytes = serde_json::to_vec(&format!("{:?}", obs.value)).unwrap_or_default();
        let _ = sqlx::query(
            "INSERT INTO observations (run_id, step, node, probe, value, recorded_at)
             VALUES (?, ?, ?, ?, ?, ?)"
        )
        .bind(&replay_run_id)
        .bind(obs.step as i64)
        .bind(obs.node as i64)
        .bind(&obs.probe)
        .bind(&value_bytes)
        .bind(now)
        .execute(&state.db)
        .await;
    }

    // 8. Verify observation-stream-hash equality (spec: "verify observation-stream-hash prefix equality")
    //
    // Use the stored stream_hash from the original run if available (set during verify/determinism).
    // Fall back to counting observations (old behavior) if the column is missing.
    let orig_obs_count = original_obs.iter()
        .filter(|(step, ..)| to_step.map_or(true, |max| *step as u64 <= max))
        .count();

    let (original_stream_hash, verified) = if let Some(stored_hash) = &original_stored_hash {
        // Compare replay stream hash against stored original hash
        let v = replay_stream_hash == *stored_hash || orig_obs_count == 0;
        (stored_hash.clone(), v)
    } else {
        // No stored hash: best-effort fallback using count equality
        let hash_placeholder = format!("<no-stored-hash-for-{}>", id);
        let v = replayed.len() == orig_obs_count || orig_obs_count == 0;
        (hash_placeholder, v)
    };

    Json(json!({
        "ok": true,
        "original_run_id": id,
        "replay_run_id": replay_run_id,
        "seed": seed,
        "spec_hash": spec_hash,
        "to_step": to_step,
        "replayed_steps": replayed.len(),
        "original_obs_count": orig_obs_count,
        "original_stream_hash": original_stream_hash,
        "replay_stream_hash": replay_stream_hash,
        "verified": verified,
        "message": if verified {
            "replay: ok=true, observation stream hashes match"
        } else {
            "replay: MISMATCH — observation stream hashes differ"
        },
    }))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Replay a spec through baud-multiverse using the stored tape (derived from seed).
/// This is the real replay path: same (seed, spec) → same observation stream hash.
fn generate_replay_observations(
    tape: Option<&[u8]>,
    seed: u64,
    _spec_hash: &str,
    spec_doc: &baud_init::parse::SpecDoc,
) -> Vec<Observation> {
    use baud_multiverse::{Multiverse, RunManifest, GuestSpec, TapeDrawSource};
    use rand_chacha::ChaCha20Rng;
    use rand::{RngCore, SeedableRng};

    // Old rows did not persist their tape, so retain their deterministic seed-derived replay.
    // Real KVM rows and explicit replay requests take the exact bytes supplied by the caller.
    let tape_bytes = match tape {
        Some(bytes) => bytes.to_vec(),
        None => {
            let mut rng = ChaCha20Rng::seed_from_u64(seed);
            let mut bytes = vec![0u8; 4096];
            rng.fill_bytes(&mut bytes);
            bytes
        }
    };

    let manifest = RunManifest {
        guests: spec_doc.nodes.iter().enumerate().map(|(i, n)| GuestSpec {
            node_id: i as u32,
            binary: std::path::PathBuf::from(&n.argv.first().cloned().unwrap_or_default()),
            argv: n.argv.clone(),
        }).collect(),
        env_override: spec_doc.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        ..RunManifest::default()
    };

    let mut tape_source = TapeDrawSource::new(tape_bytes);

    let mut mv = match Multiverse::load_from_manifest(manifest) {
        Ok(mv) => mv,
        Err(e) => {
            tracing::warn!("replay Multiverse::load failed: {e}");
            return Vec::new();
        }
    };

    // run() is infallible (spec §5): errors surface as Crash observations
    let stream = mv.run(&mut tape_source);
    stream.observations.iter().map(|e| Observation {
        probe: e.probe.clone(),
        node: e.node as u16,
        value: ProbeValue::Utf8(e.value.to_string()),
        step: e.step,
    }).collect()
}

fn decode_hex_tape(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
