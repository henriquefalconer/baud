// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /runs/raftlet — M6 raftlet fuzz loop endpoint
//
// Routes:
//   POST /runs/raftlet/fuzz → start a raftlet fuzz session (Markov weather + crash-restart)
//   GET  /runs/raftlet/:id  → get a raftlet run status
//   POST /runs/:id/raftlet/reconstruct → reconstruct a raftlet run from tape

use axum::{extract::{Path, State}, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::AppState;
use baud_driver::{Driver, StrategySpec};
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use rand::RngCore;

// ---------------------------------------------------------------------------
// Raftlet fuzz request
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RaftletFuzzBody {
    /// Path to spec or inline spec content (must reference raftlet workload)
    pub spec: String,
    /// Tactics: "random-drops", "markov-partition", or "markov-crash-restart"
    #[serde(default = "default_tactics")]
    pub tactics: String,
    /// RNG seed
    #[serde(default)]
    pub seed: u64,
    /// Max iterations
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Whether to enable the planted bug (true = raftlet has the bug)
    #[serde(default = "default_true")]
    pub planted_bug: bool,
    /// Grid strategy: maximize = ["max_commit", "max_term"]
    pub strategy: Option<String>,
}

fn default_tactics() -> String { "markov-partition".to_owned() }
fn default_max_iterations() -> u32 { 500 }
fn default_true() -> bool { true }

// ---------------------------------------------------------------------------
// POST /runs/raftlet/fuzz — run a raftlet fuzz session
// ---------------------------------------------------------------------------

pub async fn fuzz(
    State(state): State<AppState>,
    Json(body): Json<RaftletFuzzBody>,
) -> Json<Value> {
    // Lint the spec
    let spec_result = baud_init::lint(&body.spec);
    if let Err(e) = spec_result {
        return Json(json!({ "error": format!("spec lint error: {e}") }));
    }

    let spec_hash = blake3_hex(body.spec.as_bytes());
    let run_id = make_id("run");
    let now = crate::state::unix_now() as i64;

    // Insert run record
    let _ = sqlx::query(
        "INSERT INTO runs (id, spec_content, spec_hash, nix_ref, closure_hash, strategy, tactics, seed, budget_minutes, tape_id, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 'running', ?, ?)"
    )
    .bind(&run_id)
    .bind(&body.spec)
    .bind(&spec_hash)
    .bind("raftlet")
    .bind(Option::<String>::None)
    .bind(body.strategy.as_deref())
    .bind(&body.tactics)
    .bind(body.seed as i64)
    .bind(60i64)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await;

    // Run the fuzz loop in a blocking thread
    let tactics = body.tactics.clone();
    let seed = body.seed;
    let max_iterations = body.max_iterations;
    let planted_bug = body.planted_bug;
    let spec = body.spec.clone();

    let result = tokio::task::spawn_blocking(move || {
        run_raftlet_fuzz_loop(seed, &tactics, max_iterations, planted_bug, &spec)
    })
    .await;

    let fuzz_result = match result {
        Ok(r) => r,
        Err(e) => return Json(json!({ "error": format!("raftlet fuzz loop panic: {e}") })),
    };

    // Store observations
    let db = state.db.clone();
    let run_id_clone = run_id.clone();

    for (step, obs) in fuzz_result.per_gen_obs.iter().enumerate() {
        let obs_now = crate::state::unix_now() as i64;
        for (probe, value) in obs {
            let vbytes = serde_json::to_vec(&json!(*value)).unwrap_or_default();
            let _ = sqlx::query(
                "INSERT INTO observations (run_id, step, node, probe, value, recorded_at) VALUES (?, ?, 0, ?, ?, ?)"
            )
            .bind(&run_id_clone)
            .bind(step as i64)
            .bind(probe.as_str())
            .bind(&vbytes)
            .bind(obs_now)
            .execute(&db)
            .await;
        }
    }

    // Store weather events from the fuzz loop
    for event in &fuzz_result.weather_events {
        let wbytes = crate::state::unix_now() as i64;
        let _ = sqlx::query(
            "INSERT INTO net_events (run_id, step, kind, from_node, to_node, delay_ticks, drop_prob, recorded_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&run_id_clone)
        .bind(event.step as i64)
        .bind(&event.kind)
        .bind(event.from_node.map(|v| v as i64))
        .bind(event.to_node.map(|v| v as i64))
        .bind(Option::<i64>::None)
        .bind(event.drop_prob)
        .bind(wbytes)
        .execute(&db)
        .await;
    }

    // Update run status
    let final_status = if fuzz_result.violation_found { "crashed" } else { "done" };
    let _ = sqlx::query("UPDATE runs SET status = ?, updated_at = ? WHERE id = ?")
        .bind(final_status)
        .bind(crate::state::unix_now() as i64)
        .bind(&run_id_clone)
        .execute(&db)
        .await;

    // Build observation summary
    let obs_summary: Vec<Value> = fuzz_result.per_gen_obs.iter().enumerate()
        .map(|(i, obs)| {
            let mut entry = json!({ "step": i });
            for (k, v) in obs {
                entry[k] = json!(v);
            }
            entry
        })
        .collect();

    let response = json!({
        "run_id": run_id,
        "tactics": body.tactics,
        "seed": seed,
        "generations": fuzz_result.generations,
        "violation_found": fuzz_result.violation_found,
        "violation_message": fuzz_result.violation_message,
        "best_max_commit": fuzz_result.best_max_commit,
        "best_max_term": fuzz_result.best_max_term,
        "winning_tape": fuzz_result.winning_tape,
        "weather_events": fuzz_result.weather_events.len(),
        "observations": obs_summary,
        "ok": true,
        "exit_code": if fuzz_result.violation_found { 2 } else { 0 },
    });

    Json(response)
}

// ---------------------------------------------------------------------------
// GET /runs/raftlet/:id — raftlet run status
// ---------------------------------------------------------------------------

pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let row = sqlx::query_as::<_, (String, String, String, i64, i64)>(
        "SELECT id, status, tactics, seed, created_at FROM runs WHERE id = ?"
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    match row {
        Ok(Some((id, status, tactics, seed, ca))) => Json(json!({
            "run_id": id,
            "status": status,
            "tactics": tactics,
            "seed": seed,
            "created_at": ca,
        })),
        Ok(None) => Json(json!({ "error": format!("run {id} not found") })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// POST /runs/:id/raftlet/reconstruct — reconstruct from tape
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ReconstructBody {
    /// Winning tape bytes (hex-encoded)
    pub tape_hex: String,
    /// Whether to enable the planted bug
    #[serde(default = "default_true")]
    pub planted_bug: bool,
    /// Max steps
    #[serde(default = "default_reconstruct_steps")]
    pub max_steps: usize,
}

fn default_reconstruct_steps() -> usize { 300 }

pub async fn reconstruct(
    State(_state): State<AppState>,
    Path(_run_id): Path<String>,
    Json(body): Json<ReconstructBody>,
) -> Json<Value> {
    // Decode hex tape
    let tape = match hex_decode(&body.tape_hex) {
        Ok(t) => t,
        Err(e) => return Json(json!({ "error": format!("invalid tape hex: {e}") })),
    };

    let planted_bug = body.planted_bug;
    let max_steps = body.max_steps;

    let tape_len = tape.len();
    let result = tokio::task::spawn_blocking(move || {
        baud_raftlet::simulate(&tape, max_steps, planted_bug)
    })
    .await;

    match result {
        Ok((probes, violation)) => Json(json!({
            "ok": true,
            "tape_steps": tape_len / 3,
            "probes": probes,
            "violation": violation,
            "violation_found": violation.is_some(),
        })),
        Err(e) => Json(json!({ "error": format!("reconstruction panic: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// Internal fuzz loop
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct WeatherEvent {
    step: u64,
    kind: String,
    from_node: Option<u8>,
    to_node: Option<u8>,
    drop_prob: Option<f64>,
}

struct RaftletFuzzResult {
    generations: u32,
    violation_found: bool,
    violation_message: Option<String>,
    best_max_commit: f64,
    best_max_term: f64,
    winning_tape: Option<String>,
    per_gen_obs: Vec<Vec<(String, f64)>>,
    weather_events: Vec<WeatherEvent>,
}

fn run_raftlet_fuzz_loop(
    seed: u64,
    tactics: &str,
    max_iterations: u32,
    planted_bug: bool,
    _spec: &str,
) -> RaftletFuzzResult {
    let strategy = StrategySpec {
        maximize: vec!["max_commit".to_string(), "max_term".to_string()],
        buckets: vec!["max_term".to_string()],
        reservoir_keep: 32,
        reservoir_p_backoff: 0.1,
        goal_probe: Some("violation_found".to_string()),
        goal_value: Some(1.0),
    };

    let mut driver = Driver::new(seed, strategy);
    let mut rng = ChaCha20Rng::seed_from_u64(seed.wrapping_add(0xcafe_babe));

    let mut per_gen_obs = Vec::new();
    let mut weather_events: Vec<WeatherEvent> = Vec::new();
    let mut best_max_commit = 0.0f64;
    let mut best_max_term = 0.0f64;
    let mut violation_found = false;
    let mut violation_message = None;
    let mut winning_tape: Option<String> = None;

    // Markov partition state
    let mut partition_on = false;
    let mut partition_step: u64 = 0;

    for gen in 0..max_iterations {
        driver.begin_run();

        // Generate a tape for this iteration based on tactics
        let tape = generate_tape(tactics, &mut driver, &mut rng, gen,
                                 &mut partition_on, &mut partition_step,
                                 &mut weather_events);

        // Simulate the raftlet cluster
        let max_steps = 100;
        let (probes, violation) = baud_raftlet::simulate(&tape, max_steps, planted_bug);

        let max_commit = probes.get("max_commit").copied().unwrap_or(0.0);
        let max_term = probes.get("max_term").copied().unwrap_or(0.0);
        let has_leader = probes.get("has_leader").copied().unwrap_or(0.0);
        let violation_val = if violation.is_some() { 1.0f64 } else { 0.0f64 };

        // Update best
        if max_commit > best_max_commit { best_max_commit = max_commit; }
        if max_term > best_max_term { best_max_term = max_term; }

        // Record observations
        let obs = vec![
            ("max_commit".to_string(), max_commit),
            ("max_term".to_string(), max_term),
            ("has_leader".to_string(), has_leader),
            ("violation_found".to_string(), violation_val),
        ];
        per_gen_obs.push(obs.clone());

        // Report to driver
        let driver_obs: Vec<(String, f64)> = obs;
        driver.end_run(&driver_obs);

        // Check for violation
        if let Some(msg) = violation {
            violation_found = true;
            violation_message = Some(msg);
            winning_tape = Some(hex_encode(&tape));
            let _ = gen;
            return RaftletFuzzResult {
                generations: gen + 1,
                violation_found,
                violation_message,
                best_max_commit,
                best_max_term,
                winning_tape,
                per_gen_obs,
                weather_events,
            };
        }
    }

    RaftletFuzzResult {
        generations: max_iterations,
        violation_found,
        violation_message,
        best_max_commit,
        best_max_term,
        winning_tape,
        per_gen_obs,
        weather_events,
    }
}

/// Generate a tape for one fuzz iteration based on the tactics.
///
/// Each tape byte triple drives one `step_from_bytes` call in the cluster.
/// - byte[0]: action (0=tick, 1=tick, 2=deliver, 3=propose, 4=partition, 5=heal)
/// - byte[1]: parameter (which message / node)
/// - byte[2]: secondary parameter
fn generate_tape(
    tactics: &str,
    driver: &mut Driver,
    rng: &mut ChaCha20Rng,
    gen: u32,
    partition_on: &mut bool,
    partition_step: &mut u64,
    weather_events: &mut Vec<WeatherEvent>,
) -> Vec<u8> {
    let n_steps = 100usize;
    let mut tape = Vec::with_capacity(n_steps * 3);

    match tactics {
        "random-drops" => {
            // Pure random tape with some probability of partitions
            for step in 0..n_steps {
                let a = driver.draw_bits(8) as u8;
                let b = driver.draw_bits(8) as u8;
                let c = driver.draw_bits(8) as u8;
                // 10% chance of partition action
                let action = if a % 10 == 0 { 4u8 } else { a % 4 };
                tape.push(action);
                tape.push(b);
                tape.push(c);
                let _ = step;
            }
        }
        "markov-crash-restart" | "markov-partition" => {
            // Markov partition tactics: stateful partition transitions
            // p_start ≈ 0.05, p_stop ≈ 0.15
            // When partitioned: prefer deliver actions to let the cluster react
            // When not partitioned: mix of tick + propose + deliver
            for step in 0..n_steps {
                let r = (rng.next_u32() & 0xFF) as u8;

                // Markov transition
                if *partition_on {
                    let p_stop = if tactics == "markov-crash-restart" { 20u8 } else { 38u8 };
                    if r < p_stop {
                        // Heal partition
                        *partition_on = false;
                        tape.push(5u8); // heal
                        tape.push((rng.next_u32() & 0xFF) as u8);
                        tape.push((rng.next_u32() & 0xFF) as u8);
                        weather_events.push(WeatherEvent {
                            step: (gen as u64) * 100 + step as u64,
                            kind: "partition_off".to_string(),
                            from_node: Some(0),
                            to_node: Some(2),
                            drop_prob: None,
                        });
                    } else {
                        // Stay partitioned: deliver messages to make progress visible
                        tape.push(2u8); // deliver
                        tape.push((rng.next_u32() & 0xFF) as u8);
                        tape.push(0u8);
                    }
                } else {
                    let p_start = if tactics == "markov-crash-restart" { 8u8 } else { 13u8 };
                    if r < p_start {
                        // Start partition between node 0 and node 2 (leaves node 1 as tie-breaker)
                        *partition_on = true;
                        *partition_step = (gen as u64) * 100 + step as u64;
                        tape.push(4u8); // partition
                        tape.push(0u8); // node 0
                        tape.push(2u8); // node 2
                        weather_events.push(WeatherEvent {
                            step: *partition_step,
                            kind: "partition_on".to_string(),
                            from_node: Some(0),
                            to_node: Some(2),
                            drop_prob: Some(1.0),
                        });
                    } else if r < p_start + 20 {
                        // Propose a value
                        tape.push(3u8); // propose
                        tape.push((rng.next_u32() & 0xFF) as u8);
                        tape.push(0u8);
                    } else if r < p_start + 50 {
                        // Deliver a message
                        tape.push(2u8); // deliver
                        tape.push((rng.next_u32() & 0xFF) as u8);
                        tape.push(0u8);
                    } else {
                        // Tick
                        tape.push(0u8); // tick
                        tape.push(0u8);
                        tape.push(0u8);
                    }
                }
            }
        }
        _ => {
            // Default: random
            for _ in 0..n_steps {
                tape.push(driver.draw_bits(8) as u8);
                tape.push(driver.draw_bits(8) as u8);
                tape.push(driver.draw_bits(8) as u8);
            }
        }
    }

    tape
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_id(prefix: &str) -> String {
    format!("{}-{}", prefix, uuid::Uuid::new_v4().to_string().replace('-', "").chars().take(12).collect::<String>())
}

fn blake3_hex(data: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(data).to_hex())
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("odd length hex string".to_string());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}
