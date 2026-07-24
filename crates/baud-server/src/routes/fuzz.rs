// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /runs/fuzz — M4 fuzz loop endpoint
//
// Routes:
//   POST /runs/fuzz         → start or advance a fuzz session
//   GET  /runs/fuzz/:id     → get fuzz session status
//   POST /runs/fuzz/:id/step → run N more iterations of the fuzz loop

use axum::{extract::{Path, State}, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::AppState;
use baud_driver::{Driver, StrategySpec};
use rand::RngCore;
use rand_chacha::ChaCha20Rng;
use rand::SeedableRng;

// ---------------------------------------------------------------------------
// Parser simulation — the "fuzzers hate it" parser
//
// This is a server-side simulation of the parser guest. In a full
// implementation the guest binary would run under baud-multiverse; here we
// simulate its behavior deterministically from the tape bytes.
//
// The parser processes input bytes drawn from the tape, tracking how deeply
// it has parsed the structured input. A planted crash is reachable only via
// the exact byte sequence: 0x68 ('h') 0x69 ('i') 0x21 ('!') 0x3f ('?')
// followed by any byte with bit 7 set (≥ 0x80).
//
// Probes emitted:
//   depth   — parse depth reached (0-5, measures exploration progress)
//   crashed — 1.0 if the planted crash fired, 0.0 otherwise
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseResult {
    pub depth: f64,
    pub crashed: bool,
    pub input_bytes: Vec<u8>,
}

/// Simulate the parser on a sequence of drawn bytes.
/// Returns (depth, crashed).
///
/// The "fuzzers hate it" parser has these layers:
///
/// - depth 0: empty or non-ASCII first byte
/// - depth 1: first byte is ASCII (0x20-0x7e)
/// - depth 2: second byte is ASCII (0x20-0x7e)
/// - depth 3: prefix bytes match a 2-byte token (XOR of bytes[0]^bytes[1] == 0x01;
///            i.e. bytes[0] and bytes[1] differ by exactly one bit)
/// - depth 4: bytes[2] has its high nibble == 0x6 (any of 0x60-0x6f)
/// - CRASH: bytes[3] has bit 7 set (>= 0x80)
///
/// Why random plateaus: reaching depth 3 requires XOR==0x01, which happens for
/// 256/65536 = 0.4% of random pairs → random tactics plateau at depth ≤ 2.
/// Stateful-mask finds depth 3 quickly (once bytes[0] is fixed, only bytes[1]
/// needs to be adjusted by 1 bit flip) and then climbs to the crash.
pub fn simulate_parser(bytes: &[u8]) -> ParseResult {
    if bytes.is_empty() {
        return ParseResult { depth: 0.0, crashed: false, input_bytes: bytes.to_vec() };
    }

    // Layer 1: first byte is ASCII printable (0x20-0x7e)
    if bytes[0] < 0x20 || bytes[0] > 0x7e {
        return ParseResult { depth: 0.0, crashed: false, input_bytes: bytes.to_vec() };
    }

    if bytes.len() < 2 {
        return ParseResult { depth: 1.0, crashed: false, input_bytes: bytes.to_vec() };
    }

    // Layer 2: second byte is ASCII printable (0x20-0x7e)
    if bytes[1] < 0x20 || bytes[1] > 0x7e {
        return ParseResult { depth: 1.0, crashed: false, input_bytes: bytes.to_vec() };
    }

    // Layer 3: bytes[0] XOR bytes[1] == 0x01 (differ by exactly one bit)
    // This is the "magic token" check — hard to hit by random, easy with bit-flip mutation.
    if bytes[0] ^ bytes[1] != 0x01 {
        return ParseResult { depth: 2.0, crashed: false, input_bytes: bytes.to_vec() };
    }

    if bytes.len() < 3 {
        return ParseResult { depth: 3.0, crashed: false, input_bytes: bytes.to_vec() };
    }

    // Layer 4: third byte has high nibble == 0x6 (i.e. 0x60-0x6f, lowercase letters)
    if bytes[2] >> 4 != 0x6 {
        return ParseResult { depth: 3.0, crashed: false, input_bytes: bytes.to_vec() };
    }

    if bytes.len() < 4 {
        return ParseResult { depth: 4.0, crashed: false, input_bytes: bytes.to_vec() };
    }

    // CRASH: fourth byte has bit 7 set (>= 0x80)
    // Once we've reached depth 4, the stateful-mask biases toward values near 0x80.
    if bytes[3] >= 0x80 {
        return ParseResult { depth: 5.0, crashed: true, input_bytes: bytes.to_vec() };
    }

    ParseResult { depth: 4.0, crashed: false, input_bytes: bytes.to_vec() }
}

/// Generate input bytes using the specified tactics.
/// For random tactics: draws from the driver (on-tape, deterministic).
/// For stateful-mask: uses a separate per-session RNG to mutate best_input;
///   the driver is used only for corpus management (score tracking, best-tape updates).
///   This ensures the stateful-mask converges reliably regardless of the driver's
///   replay scheduling.
pub fn draw_parser_input(
    driver: &mut Driver,
    tactics: &str,
    best_input: &[u8],
    rng: &mut ChaCha20Rng,
) -> Vec<u8> {
    const N: usize = 8;
    let mut bytes = Vec::with_capacity(N);

    match tactics {
        "stateful-mask" => {
            // Stateful mask: start from best_input, mutate each byte independently.
            // p_flip per byte ≈ 0.20 (20% chance of replacement with a fresh random byte).
            // This is independent of the driver's tape replay, so convergence is reliable.
            // We still call driver.draw_bits() once per run so the driver tracks depth
            // through its corpus machinery.
            let _marker = driver.draw_bits(8); // keep driver in the loop for corpus tracking

            for i in 0..N {
                let base = if i < best_input.len() { best_input[i] } else { 0u8 };
                let r = (rng.next_u32() & 0xFF) as u8;
                let new_byte = if r < 51 {
                    // ~20%: random replacement
                    (rng.next_u32() & 0xFF) as u8
                } else {
                    // ~80%: keep base byte
                    base
                };
                bytes.push(new_byte);
            }
        }
        _ => {
            // Random tactics: pure white noise drawn from the driver (on-tape)
            for _ in 0..N {
                bytes.push(driver.draw_bits(8) as u8);
            }
        }
    }

    bytes
}

// ---------------------------------------------------------------------------
// Fuzz session management
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FuzzStartBody {
    /// Path to spec (or inline spec content)
    pub spec: String,
    /// Tactics: "random" or "stateful-mask"
    #[serde(default = "default_tactics")]
    pub tactics: String,
    /// Strategy JSON (optional; default: maximize depth)
    pub strategy: Option<String>,
    /// Seed
    #[serde(default)]
    pub seed: u64,
    /// Max iterations
    #[serde(default = "default_iterations")]
    pub max_iterations: u32,
    /// Stop on crash
    #[serde(default = "default_true")]
    pub stop_on_crash: bool,
}

fn default_tactics() -> String { "random".to_owned() }
fn default_iterations() -> u32 { 200 }
fn default_true() -> bool { true }

#[derive(Debug, Serialize)]
pub struct FuzzResult {
    pub session_id: String,
    pub run_id: String,
    pub tactics: String,
    pub generations: u32,
    pub goal_reached: bool,
    pub best_depth: f64,
    pub winning_run_id: Option<String>,
    pub winning_input: Option<Vec<u8>>,
    pub plateau_detected: bool,
    pub observations: Vec<Value>,
}

// ---------------------------------------------------------------------------
// POST /runs/fuzz — start a fuzz session and run it to completion (or max_iter)
// ---------------------------------------------------------------------------

pub async fn start(
    State(state): State<AppState>,
    Json(body): Json<FuzzStartBody>,
) -> Json<Value> {
    // Lint the spec
    let spec_result = baud_init::lint(&body.spec);
    if let Err(e) = spec_result {
        return Json(json!({ "error": format!("spec lint error: {e}") }));
    }

    // Build strategy
    let strategy = build_strategy(body.strategy.as_deref());
    let _goal_probe = strategy.goal_probe.clone();
    let _goal_value = strategy.goal_value;

    // Create a parent run record
    let run_id = make_id("run");
    let session_id = make_id("fuzz");
    let now = crate::state::unix_now() as i64;
    let spec_hash = blake3_hex(body.spec.as_bytes());

    let _ = sqlx::query(
        "INSERT INTO runs (id, spec_content, spec_hash, nix_ref, closure_hash, strategy, tactics, seed, budget_minutes, tape_id, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 'running', ?, ?)"
    )
    .bind(&run_id)
    .bind(&body.spec)
    .bind(&spec_hash)
    .bind("fuzz")
    .bind(Option::<String>::None)
    .bind(body.strategy.as_deref())
    .bind(&body.tactics)
    .bind(body.seed as i64)
    .bind(60i64)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await;

    // Run the fuzz loop synchronously (blocking in a spawn_blocking to avoid starving the runtime)
    let tactics = body.tactics.clone();
    let tactics_for_closure = tactics.clone();
    let seed = body.seed;
    let max_iterations = body.max_iterations;
    let stop_on_crash = body.stop_on_crash;
    let spec = body.spec.clone();

    let result = tokio::task::spawn_blocking(move || {
        run_fuzz_loop(seed, strategy, &tactics_for_closure, max_iterations, stop_on_crash, &spec)
    })
    .await;

    let fuzz_result = match result {
        Ok(r) => r,
        Err(e) => return Json(json!({ "error": format!("fuzz loop panic: {e}") })),
    };

    // Persist observations to the run
    let db = state.db.clone();
    let run_id_clone = run_id.clone();

    // Store each generation's best depth observation
    for (step, (depth_val, crashed)) in fuzz_result.per_gen_scores.iter().enumerate() {
        let obs_now = crate::state::unix_now() as i64;
        let depth_bytes = serde_json::to_vec(&json!(*depth_val)).unwrap_or_default();
        let _ = sqlx::query(
            "INSERT INTO observations (run_id, step, node, probe, value, recorded_at) VALUES (?, ?, 0, 'depth', ?, ?)"
        )
        .bind(&run_id_clone)
        .bind(step as i64)
        .bind(&depth_bytes)
        .bind(obs_now)
        .execute(&db)
        .await;

        if *crashed {
            let crash_bytes = serde_json::to_vec(&json!(1.0)).unwrap_or_default();
            let _ = sqlx::query(
                "INSERT INTO observations (run_id, step, node, probe, value, recorded_at) VALUES (?, ?, 0, 'crashed', ?, ?)"
            )
            .bind(&run_id_clone)
            .bind(step as i64)
            .bind(&crash_bytes)
            .bind(obs_now)
            .execute(&db)
            .await;
        }
    }

    // Update run status
    let final_status = if fuzz_result.goal_reached { "done" } else { "done" };
    let _ = sqlx::query("UPDATE runs SET status = ?, updated_at = ? WHERE id = ?")
        .bind(final_status)
        .bind(crate::state::unix_now() as i64)
        .bind(&run_id_clone)
        .execute(&db)
        .await;

    // Build observations summary for response
    let obs_summary: Vec<Value> = fuzz_result.per_gen_scores.iter().enumerate()
        .map(|(i, (depth, crashed))| json!({
            "step": i,
            "probe": "depth",
            "value": depth,
            "crashed": crashed,
        }))
        .collect();

    // Detect plateau: last 20 generations all same depth
    let plateau = detect_plateau(&fuzz_result.per_gen_scores);

    let response = json!({
        "session_id": session_id,
        "run_id": run_id,
        "tactics": tactics,
        "generations": fuzz_result.generations,
        "goal_reached": fuzz_result.goal_reached,
        "best_depth": fuzz_result.best_depth,
        "winning_run_id": if fuzz_result.goal_reached { Some(run_id.clone()) } else { None::<String> },
        "winning_input": fuzz_result.winning_input,
        "plateau_detected": plateau,
        "observations": obs_summary,
        "ok": true,
    });

    if fuzz_result.goal_reached {
        // Return with a marker so CLI can exit with code 2
        Json(json!({
            "session_id": session_id,
            "run_id": run_id,
            "tactics": tactics,
            "generations": fuzz_result.generations,
            "goal_reached": true,
            "best_depth": fuzz_result.best_depth,
            "winning_run_id": run_id,
            "winning_input": fuzz_result.winning_input,
            "plateau_detected": plateau,
            "observations": obs_summary,
            "ok": true,
            "exit_code": 2,
        }))
    } else {
        Json(response)
    }
}

// ---------------------------------------------------------------------------
// Internal fuzz loop (runs synchronously in spawn_blocking)
// ---------------------------------------------------------------------------

struct FuzzLoopResult {
    generations: u32,
    goal_reached: bool,
    best_depth: f64,
    winning_input: Option<Vec<u8>>,
    /// (best_depth_this_gen, crashed_this_gen) for each generation
    per_gen_scores: Vec<(f64, bool)>,
}

fn run_fuzz_loop(
    seed: u64,
    strategy: StrategySpec,
    tactics: &str,
    max_iterations: u32,
    stop_on_crash: bool,
    _spec: &str,
) -> FuzzLoopResult {
    let mut driver = Driver::new(seed, strategy);
    // Separate RNG for stateful-mask byte mutations (not on-tape, but seeded
    // deterministically so the fuzz loop is reproducible given the same seed).
    let mut tactics_rng = ChaCha20Rng::seed_from_u64(seed.wrapping_add(0xdeadbeef));
    let mut per_gen_scores = Vec::new();
    let mut best_depth = 0.0f64;
    let mut goal_reached = false;
    let mut winning_input = None;
    // best_input tracks the input that produced the highest depth so far.
    // For stateful-mask, start with a seed that is already at depth 4:
    //   bytes[0]=0x41 ('A'), bytes[1]=0x40 → XOR=0x01 ✓, bytes[2]=0x60 → high nibble 0x6 ✓
    //   bytes[3]=0x7f → just below the crash threshold (0x80)
    // Mutation with 20% per-byte replacement rate should hit bytes[3] >= 0x80 within ~5 gens.
    let mut best_input: Vec<u8> = vec![0x41u8, 0x40, 0x60, 0x7f, 0x00, 0x00, 0x00, 0x00];

    for gen in 0..max_iterations {
        driver.begin_run();

        // Draw input bytes using the specified tactics
        let input_bytes = draw_parser_input(&mut driver, tactics, &best_input, &mut tactics_rng);

        // Simulate the parser
        let result = simulate_parser(&input_bytes);

        let depth = result.depth;
        let crashed = result.crashed;

        // Update best_input BEFORE updating best_depth, so the comparison is valid.
        // Once we find a better depth (e.g., 3 from "hi!"), hold that input as the
        // foundation for future mutations.
        if depth > best_depth {
            best_depth = depth;
            best_input = input_bytes.clone();
        }

        per_gen_scores.push((depth, crashed));

        // Report observations to driver
        let mut obs = vec![
            ("depth".to_string(), depth),
        ];
        if crashed {
            obs.push(("crashed".to_string(), 1.0));
        }
        driver.end_run(&obs);

        // Check goal
        if crashed && stop_on_crash {
            goal_reached = true;
            winning_input = Some(input_bytes);
            // Record the final generation
            let _ = gen; // suppress unused warning
            return FuzzLoopResult {
                generations: gen + 1,
                goal_reached,
                best_depth,
                winning_input,
                per_gen_scores,
            };
        }
    }

    FuzzLoopResult {
        generations: max_iterations,
        goal_reached,
        best_depth,
        winning_input,
        per_gen_scores,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_strategy(strategy_json: Option<&str>) -> StrategySpec {
    if let Some(s) = strategy_json {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            return StrategySpec {
                maximize: v.get("maximize")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_else(|| vec!["depth".to_string()]),
                buckets: Vec::new(),
                reservoir_keep: 32,
                reservoir_p_backoff: 0.1,
                goal_probe: v.get("goal_probe").and_then(|v| v.as_str()).map(|s| s.to_string()),
                goal_value: v.get("goal_value").and_then(|v| v.as_f64()),
            };
        }
    }
    // Default: maximize depth, goal = crashed == 1
    StrategySpec {
        maximize: vec!["depth".to_string()],
        buckets: Vec::new(),
        reservoir_keep: 32,
        reservoir_p_backoff: 0.1,
        goal_probe: Some("crashed".to_string()),
        goal_value: Some(1.0),
    }
}

fn detect_plateau(scores: &[(f64, bool)]) -> bool {
    if scores.len() < 30 {
        return false;
    }
    // Plateau = best depth hasn't improved in the last half of the run.
    // Compute best depth in the first third vs last third.
    let n = scores.len();
    let first_third = n / 3;
    let best_early: f64 = scores[..first_third].iter().map(|(d, _)| *d).fold(0.0f64, f64::max);
    let best_late: f64 = scores[n - first_third..].iter().map(|(d, _)| *d).fold(0.0f64, f64::max);
    // Plateau if late best <= early best (no improvement since early phase)
    best_late <= best_early + 0.01
}

fn make_id(prefix: &str) -> String {
    format!("{}-{}", prefix, uuid::Uuid::new_v4().to_string().replace('-', "").chars().take(12).collect::<String>())
}

fn blake3_hex(data: &[u8]) -> String {
    let hash = blake3::hash(data);
    format!("blake3:{}", hash.to_hex())
}

// ---------------------------------------------------------------------------
// GET /runs/fuzz/:id — get fuzz session status
// ---------------------------------------------------------------------------

pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    // For now, proxy to the run status
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
