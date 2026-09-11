// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /replay — run replay routes (M3)
//
// Routes:
//   POST /replay/:id         → replay a run (from stored tape/journal)
//   POST /replay/:id/to-step → replay up to a given step

use crate::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use baud_proto::{Observation, Value as ProbeValue};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct ReplayBody {
    /// Optional tape file contents (CBOR-encoded Tape); if absent, uses stored tape
    #[allow(dead_code)]
    pub tape_bytes: Option<Vec<u8>>,
    /// Replay up to this step (inclusive); if absent, replay full run
    pub to_step: Option<u64>,
}

/// POST /replay/:id — replay a run
type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn api_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message.into() })))
}

pub async fn replay(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<Option<ReplayBody>>,
) -> ApiResult {
    let body = body.unwrap_or(ReplayBody {
        tape_bytes: None,
        to_step: None,
    });

    // 1. Look up the run
    let row = sqlx::query_as::<_, (String, String, String, Option<String>, i64, String, Option<String>)>(
        "SELECT id, spec_content, spec_hash, closure_hash, seed, status, stream_hash FROM runs WHERE id = ?"
    )
    .bind(&run_id)
    .fetch_optional(&state.db)
    .await;

    let (id, spec_content, spec_hash, closure_hash, seed, status, original_stored_hash) = match row
    {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err(api_error(
                StatusCode::NOT_FOUND,
                format!("run {run_id} not found"),
            ))
        }
        Err(e) => {
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("db error: {e}"),
            ))
        }
    };

    // Guard: divergent runs are excluded from replay (spec baud-journal §5 / VR2-M15).
    if status == "divergent" {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": format!("run {run_id} is marked divergent and cannot be replayed"),
                "status": "divergent",
            })),
        ));
    }

    // 2. Parse spec to understand topology
    let spec_doc = match baud_init::lint(&spec_content) {
        Ok(doc) => doc,
        Err(e) => {
            return Err(api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("spec parse error: {e}"),
            ))
        }
    };

    // 3. Create a replay run record
    let replay_run_id = format!(
        "replay-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .replace('-', "")
            .chars()
            .take(8)
            .collect::<String>()
    );
    let now = crate::state::unix_now() as i64;
    let replay_spec_hash = spec_hash.clone();

    sqlx::query(
        "INSERT INTO runs (id, spec_content, spec_hash, nix_ref, closure_hash, strategy, tactics, seed, budget_minutes, tape_id, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, 5, NULL, 'running', ?, ?)"
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
    .await
    .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("replay journal error: {e}")))?;

    // 4. Fetch original observations from the run being replayed
    let original_rows = sqlx::query_as::<_, (i64, i64, String, Vec<u8>, i64)>(
        "SELECT step, node, probe, value, recorded_at FROM observations
         WHERE run_id = ?
         ORDER BY step ASC",
    )
    .bind(&run_id)
    .fetch_all(&state.db)
    .await;

    let original_obs = match original_rows {
        Ok(r) => r,
        Err(e) => {
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("db error fetching observations: {e}"),
            ))
        }
    };

    // 5. Prefer the exact tape recorded by a real KVM run. Re-seeding a PRNG here would
    // silently replay a different input stream, which is especially easy to miss when the
    // guest consumes only a prefix. The legacy seed path remains for pre-KVM rows that have no
    // persisted tape metadata.
    let stored_tape =
        sqlx::query_as::<_, (String,)>("SELECT tape_hex FROM kvm_run_meta WHERE run_id = ?")
            .bind(&run_id)
            .fetch_optional(&state.db)
            .await;
    let replay_tape = match stored_tape {
        Ok(Some((tape_hex,))) => match decode_hex_tape(&tape_hex) {
            Some(tape) => Some(tape),
            None => {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({
                        "ok": false,
                        "verified": false,
                        "error": "stored KVM tape is malformed"
                    })),
                ))
            }
        },
        Ok(None) => body.tape_bytes.filter(|tape| !tape.is_empty()),
        Err(e) => {
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("db error fetching replay tape: {e}"),
            ))
        }
    };
    let replay_obs = match replay_real_or_legacy(
        &state,
        &run_id,
        replay_tape.as_deref(),
        seed as u64,
        &spec_hash,
        &spec_doc,
    )
    .await
    {
        Ok(observations) => observations,
        Err(error) => {
            sqlx::query("UPDATE runs SET status = 'failed', updated_at = ? WHERE id = ?")
                .bind(now)
                .bind(&replay_run_id)
                .execute(&state.db)
                .await
                .map_err(|e| {
                    api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("replay failure journal error: {e}"),
                    )
                })?;
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "ok": false,
                    "verified": false,
                    "original_run_id": id,
                    "error": error,
                    "message": "replay failed before an observation stream was produced"
                })),
            ));
        }
    };

    let to_step = body.to_step;
    let mut replayed = Vec::new();
    let mut replay_hash = blake3::Hasher::new();

    for obs in &replay_obs {
        if let Some(max) = to_step {
            if obs.step > max {
                break;
            }
        }
        let obs_cbor =
            baud_proto::encode(&baud_proto::Msg::Observe(obs.clone())).unwrap_or_default();
        replay_hash.update(&obs_cbor);
        replayed.push(obs.clone());
    }

    let replay_stream_hash = hex_encode(replay_hash.finalize().as_bytes());

    // A replay with no observations is a failed execution, not a successful empty stream.
    // Keep the diagnostic response, but never let it become a verified replay.
    // 7. Insert replayed observations into SQLite under replay_run_id
    for obs in &replayed {
        let value_bytes = serde_json::to_vec(&obs.value).map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("observation encoding error: {e}"),
            )
        })?;
        sqlx::query(
            "INSERT INTO observations (run_id, step, node, probe, value, recorded_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&replay_run_id)
        .bind(obs.step as i64)
        .bind(obs.node as i64)
        .bind(&obs.probe)
        .bind(&value_bytes)
        .bind(now)
        .execute(&state.db)
        .await
        .map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("replay observation journal error: {e}"),
            )
        })?;
    }

    // 8. Verify observation-stream-hash equality (spec: "verify observation-stream-hash prefix equality")
    //
    // Use the stored stream_hash from the original run if available (set during verify/determinism).
    // Fall back to counting observations (old behavior) if the column is missing.
    let orig_obs_count = original_obs
        .iter()
        .filter(|(step, ..)| to_step.map_or(true, |max| *step as u64 <= max))
        .count();

    let (original_stream_hash, verified) = if let Some(stored_hash) = &original_stored_hash {
        // A full-run hash cannot verify a prefix replay. For `to_step`, hash the exact original
        // prefix from the durable observation rows; for a full replay, retain the stored hash as
        // the authority. This prevents a truncated replay from being reported as a mismatch merely
        // because it was intentionally bounded, while still rejecting any changed observation.
        let expected_hash = if to_step.is_some() {
            hash_observation_prefix(&original_obs, to_step)
        } else {
            stored_hash.clone()
        };
        let v = replay_stream_hash == expected_hash && !replayed.is_empty();
        (expected_hash, v)
    } else {
        // Older rows predate the persisted stream_hash column, but their observation rows still
        // contain the exact protocol values. Hash those rows instead of returning a placeholder or
        // treating equal counts as proof. A legacy replay is verified only when its encoded stream
        // matches that reconstructed hash and is non-empty.
        let expected_hash = hash_observation_prefix(&original_obs, to_step);
        let v = replay_stream_hash == expected_hash && !replayed.is_empty();
        (expected_hash, v)
    };

    let terminal_status = if verified { "done" } else { "failed" };
    sqlx::query("UPDATE runs SET status = ?, updated_at = ? WHERE id = ?")
        .bind(terminal_status)
        .bind(now)
        .bind(&replay_run_id)
        .execute(&state.db)
        .await
        .map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("replay completion journal error: {e}"),
            )
        })?;
    if !verified {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "ok": false,
                "original_run_id": id,
                "replay_run_id": replay_run_id,
                "verified": false,
                "replayed_steps": replayed.len(),
                "original_obs_count": orig_obs_count,
                "original_stream_hash": original_stream_hash,
                "replay_stream_hash": replay_stream_hash,
                "message": "replay: observation stream hashes differ"
            })),
        ));
    }

    Ok(Json(json!({
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
    })))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Prefer the persisted real-KVM boot contract. Legacy rows without `kvm_run_meta` have no
/// image to boot, so they use the deterministic compatibility engine and are reported as legacy
/// replay by the response rather than being mistaken for a real KVM boot.
async fn replay_real_or_legacy(
    state: &AppState,
    run_id: &str,
    tape: Option<&[u8]>,
    seed: u64,
    spec_hash: &str,
    spec_doc: &baud_init::parse::SpecDoc,
) -> Result<Vec<Observation>, String> {
    let meta = sqlx::query_as::<_, (
        String, String, Option<String>, Option<i64>, Option<i64>, Option<i64>,
        Option<i64>, Option<i64>, Option<i64>, bool, Option<String>, Option<i64>, Option<i64>
    )>(
        "SELECT kernel_path, cmdline, initramfs_path, periodic_timer_period_rcb, \
         periodic_timer_vector, periodic_timer_max_ticks, virtio_rng_seed, virtio_rng_vector, \
         virtio_rng_max_exits, acpi, virtio_blk_image_path, virtio_blk_vector, virtio_blk_max_exits \
         FROM kvm_run_meta WHERE run_id = ?"
    )
    .bind(run_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| format!("db error fetching real replay metadata: {e}"))?;

    let Some((
        kernel,
        cmdline,
        initramfs_path,
        period,
        timer_vector,
        max_ticks,
        rng_seed,
        rng_vector,
        rng_max_exits,
        acpi,
        blk_path,
        blk_vector,
        blk_max_exits,
    )) = meta
    else {
        return generate_replay_observations(tape, seed, spec_hash, spec_doc);
    };
    let tape = tape
        .ok_or_else(|| "real replay input tape is unavailable".to_owned())?
        .to_vec();
    let initramfs = initramfs_path
        .as_deref()
        .map(crate::routes::run_kvm::read_initramfs)
        .transpose()?;
    let periodic = period
        .zip(timer_vector)
        .zip(max_ticks)
        .map(|((period, vector), max)| (period as u64, vector as u8, max as u32));
    let rng = rng_seed
        .zip(rng_vector)
        .zip(rng_max_exits)
        .map(|((seed, vector), max)| (seed as u64, vector as u8, max as u32));
    let blk = match (blk_path, blk_vector, blk_max_exits) {
        (Some(path), Some(vector), Some(max)) => Some((
            crate::routes::run_kvm::open_virtio_blk_image(
                &path,
                crate::routes::run_kvm::virtio_blk_image_size_limit(),
            )?,
            vector as u8,
            max as u32,
        )),
        (None, None, None) => None,
        _ => return Err("real replay metadata has an incomplete virtio-blk configuration".into()),
    };
    let records = tokio::task::spawn_blocking(move || {
        crate::routes::run_kvm::replay_real_records(
            std::path::Path::new(&kernel),
            &cmdline,
            tape,
            initramfs.as_deref(),
            periodic,
            rng,
            blk,
            acpi,
        )
    })
    .await
    .map_err(|e| format!("real replay task failed: {e}"))??;
    Ok(records
        .into_iter()
        .filter_map(|record| match record {
            baud_proto::Msg::Observe(observation) => Some(observation),
            _ => None,
        })
        .collect())
}

/// Replay a spec through the legacy deterministic engine using the stored tape.
/// This is the real replay path: same (seed, spec) → same observation stream hash.
fn generate_replay_observations(
    tape: Option<&[u8]>,
    seed: u64,
    _spec_hash: &str,
    spec_doc: &baud_init::parse::SpecDoc,
) -> Result<Vec<Observation>, String> {
    use baud_multiverse::{GuestSpec, Multiverse, RunManifest, TapeDrawSource};
    use rand::{RngCore, SeedableRng};
    use rand_chacha::ChaCha20Rng;

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
        guests: spec_doc
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| GuestSpec {
                node_id: i as u32,
                binary: std::path::PathBuf::from(&n.argv.first().cloned().unwrap_or_default()),
                argv: n.argv.clone(),
                binary_hash: String::new(),
            })
            .collect(),
        env_override: spec_doc
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        ..RunManifest::default()
    };

    let mut tape_source = TapeDrawSource::new(tape_bytes);

    let mut mv = match Multiverse::load_from_manifest(manifest) {
        Ok(mv) => mv,
        Err(e) => return Err(format!("multiverse failed to load the workload: {e}")),
    };

    // run() is infallible (spec §5): errors surface as Crash observations
    let stream = mv.run(&mut tape_source);
    Ok(stream
        .observations
        .iter()
        .map(|e| Observation {
            probe: e.probe.clone(),
            node: e.node as u16,
            value: ProbeValue::Utf8(e.value.to_string()),
            step: e.step,
        })
        .collect())
}

fn hash_observation_prefix(
    rows: &[(i64, i64, String, Vec<u8>, i64)],
    to_step: Option<u64>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for (step, node, probe, value, _) in rows {
        if to_step.is_some_and(|limit| *step as u64 > limit) {
            break;
        }
        let typed_value = serde_json::from_slice(value)
            .unwrap_or_else(|_| ProbeValue::Utf8(String::from_utf8_lossy(value).into_owned()));
        let observation = Observation {
            probe: probe.clone(),
            node: *node as u16,
            value: typed_value,
            step: *step as u64,
        };
        if let Ok(encoded) = baud_proto::encode(&baud_proto::Msg::Observe(observation)) {
            hasher.update(&encoded);
        }
    }
    hex_encode(hasher.finalize().as_bytes())
}

fn decode_hex_tape(s: &str) -> Option<Vec<u8>> {
    if !s.is_ascii() || !s.len().is_multiple_of(2) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_hash_matches_the_protocol_observation_encoding() {
        let rows = vec![
            (
                0,
                2,
                "banner".to_owned(),
                serde_json::to_vec(&ProbeValue::Utf8("ready".into())).unwrap(),
                0,
            ),
            (
                1,
                2,
                "score".to_owned(),
                serde_json::to_vec(&ProbeValue::U64(7)).unwrap(),
                0,
            ),
        ];
        let mut expected = blake3::Hasher::new();
        for (step, node, probe, value, _) in &rows {
            let observation = Observation {
                probe: probe.clone(),
                node: *node as u16,
                value: serde_json::from_slice(value).unwrap(),
                step: *step as u64,
            };
            expected.update(&baud_proto::encode(&baud_proto::Msg::Observe(observation)).unwrap());
        }
        assert_eq!(
            hash_observation_prefix(&rows, None),
            hex_encode(expected.finalize().as_bytes())
        );
        assert_ne!(
            hash_observation_prefix(&rows, Some(0)),
            hash_observation_prefix(&rows, None)
        );
    }

    #[test]
    fn malformed_hex_tape_is_rejected() {
        assert!(decode_hex_tape("0").is_none());
        assert!(decode_hex_tape("zz").is_none());
        assert!(decode_hex_tape("0€").is_none());
        assert!(decode_hex_tape("é").is_none());
        assert_eq!(decode_hex_tape("00ff"), Some(vec![0, 255]));
    }
}
