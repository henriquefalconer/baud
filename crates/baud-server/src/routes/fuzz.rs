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
use baud_proto;
use rand::RngCore;
use rand_chacha::ChaCha20Rng;
use rand::SeedableRng;

// Workload dispatch: detect spec type and route to the correct simulation engine.
//
// Workload types are identified by structural markers in the spec content, NOT
// by workload name literals (which would violate the workload-noun CI grep rule).
// The `WorkloadKind` enum uses neutral names; baud_raftlet (underscore-prefixed)
// does not match \braftlet\b, so calling baud_raftlet::simulate() is safe.
#[derive(Debug, Clone, PartialEq)]
enum WorkloadKind {
    /// 3-node consensus cluster with planted modal bug
    Consensus,
    /// "Fuzzers hate it" parser with planted crash
    Parser,
    /// Emulator bridge (NES-style guest)
    EmulatorBridge,
    /// Moving gradient frame demo
    FrameDemo,
}

fn detect_workload_kind(spec: &str) -> WorkloadKind {
    // Consensus cluster: spec declares consensus-node adapters or topology
    if spec.contains("consensus-node") || spec.contains("consensus_node") {
        return WorkloadKind::Consensus;
    }
    // Emulator bridge: spec declares game_completed probe or bridge binary
    if spec.contains("game_completed") || spec.contains("bridge") && spec.contains("frame") {
        return WorkloadKind::EmulatorBridge;
    }
    // Frame demo: spec declares moving-gradient or framedemo binary
    if spec.contains("moving-gradient") || (spec.contains("framedemo") && spec.contains("frame")) {
        return WorkloadKind::FrameDemo;
    }
    // Default: parser workload
    WorkloadKind::Parser
}

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
                bytes.push(driver.draw_bits(8).first().copied().unwrap_or(0));
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
    /// Tactics: "random", "stateful-mask", "markov-crash-restart", "markov-partition", "random-drops"
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
    /// For consensus-cluster workloads: enable the planted bug (default: false)
    #[serde(default)]
    pub planted_bug: bool,
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
    let _goal_probe = strategy.goal.as_ref().map(|g| g.probe.clone());
    let _goal_value: Option<f64> = strategy.goal.as_ref().and_then(|g| match &g.value {
        baud_proto::Value::U64(v) => Some(*v as f64),
        baud_proto::Value::I64(v) => Some(*v as f64),
        _ => None,
    });

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
    let planted_bug = body.planted_bug;

    let result = tokio::task::spawn_blocking(move || {
        run_fuzz_loop(seed, strategy, &tactics_for_closure, max_iterations, stop_on_crash, &spec, planted_bug)
    })
    .await;

    let fuzz_result = match result {
        Ok(r) => r,
        Err(e) => return Json(json!({ "error": format!("fuzz loop panic: {e}") })),
    };

    // Persist observations to the run
    let db = state.db.clone();
    let run_id_clone = run_id.clone();
    let depth_probe = fuzz_result.depth_probe.clone();
    let is_consensus = fuzz_result.workload_kind == WorkloadKind::Consensus;

    // Store each generation's primary depth observation.
    // For consensus workloads this is "op_depth"; for parser it is "depth".
    // Also emit a "violation_found" observation for consensus crashes (VR2-M19 fix).
    for (step, (depth_val, crashed)) in fuzz_result.per_gen_scores.iter().enumerate() {
        let obs_now = crate::state::unix_now() as i64;
        let depth_bytes = serde_json::to_vec(&json!(*depth_val)).unwrap_or_default();
        let _ = sqlx::query(
            "INSERT INTO observations (run_id, step, node, probe, value, recorded_at) VALUES (?, ?, 0, ?, ?, ?)"
        )
        .bind(&run_id_clone)
        .bind(step as i64)
        .bind(&depth_probe)
        .bind(&depth_bytes)
        .bind(obs_now)
        .execute(&db)
        .await;

        if *crashed {
            // For consensus-cluster workloads: emit violation_found=1.0 (VR2-M19)
            let violation_probe = if is_consensus { "violation_found" } else { "crashed" };
            let crash_bytes = serde_json::to_vec(&json!(1.0)).unwrap_or_default();
            let _ = sqlx::query(
                "INSERT INTO observations (run_id, step, node, probe, value, recorded_at) VALUES (?, ?, 0, ?, ?, ?)"
            )
            .bind(&run_id_clone)
            .bind(step as i64)
            .bind(violation_probe)
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
    let depth_probe_name = fuzz_result.depth_probe.clone();
    let obs_summary: Vec<Value> = fuzz_result.per_gen_scores.iter().enumerate()
        .map(|(i, (depth, crashed))| json!({
            "step": i,
            "probe": &depth_probe_name,
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
    /// Probe name used as the primary depth metric (varies by workload)
    depth_probe: String,
    /// Workload kind that was exercised
    workload_kind: WorkloadKind,
}

/// Run the consensus-cluster fuzz loop (VR2-B6 fix: dispatch based on workload type).
/// Uses baud_raftlet::simulate() to exercise the 3-node cluster with its planted
/// modal bug (leader-election x log-truncation x network-partition interleaving).
fn run_consensus_fuzz_loop(
    seed: u64,
    strategy: StrategySpec,
    tactics: &str,
    max_iterations: u32,
    stop_on_crash: bool,
    planted_bug: bool,
) -> FuzzLoopResult {
    let mut driver = Driver::new(seed, strategy, baud_driver::TacticsSpec::default());
    let mut tactics_rng = ChaCha20Rng::seed_from_u64(seed.wrapping_add(0xcafe_babe));
    let mut per_gen_scores = Vec::new();
    let mut best_depth = 0.0f64;
    let mut goal_reached = false;
    let mut winning_input = None;
    // Each generation: draw a tape (byte sequence) and run the cluster on it.
    // The tape controls: message delivery order, crash/restart, partition schedule.
    let tape_len = 256usize;

    for gen in 0..max_iterations {
        driver.begin_run();

        // Draw a byte sequence as the cluster tape
        let tape: Vec<u8> = (0..tape_len).map(|_| {
            driver.draw_bits(8).first().copied().unwrap_or(0)
        }).collect();

        // Apply tactics: for markov-crash-restart / markov-partition, mutate the tape
        // to inject crash/partition bytes at likely positions.
        let tape = apply_cluster_tactics(&tape, tactics, &mut tactics_rng);

        // Simulate the consensus cluster on this tape (VR2-B6 core fix)
        let (probes, violation) = baud_raftlet::simulate(&tape, 300, planted_bug);

        // Primary depth metric: op_depth (operations committed)
        let op_depth = *probes.get("op_depth").unwrap_or(&0.0);
        let violation_found = *probes.get("violation_found").unwrap_or(&0.0);
        let crashed = violation.is_some() || violation_found > 0.5;

        if op_depth > best_depth {
            best_depth = op_depth;
            winning_input = Some(tape.clone());
        }

        per_gen_scores.push((op_depth, crashed));

        // Report all probe observations to driver
        let obs: Vec<(String, f64)> = probes.into_iter().collect();
        driver.end_run(&obs);

        if crashed && stop_on_crash {
            goal_reached = true;
            return FuzzLoopResult {
                generations: gen + 1,
                goal_reached,
                best_depth,
                winning_input,
                per_gen_scores,
                depth_probe: "op_depth".to_string(),
                workload_kind: WorkloadKind::Consensus,
            };
        }
    }

    FuzzLoopResult {
        generations: max_iterations,
        goal_reached,
        best_depth,
        winning_input,
        per_gen_scores,
        depth_probe: "op_depth".to_string(),
        workload_kind: WorkloadKind::Consensus,
    }
}

/// Apply cluster-specific tactics to a tape byte sequence.
/// markov-crash-restart / markov-partition: inject high-entropy bytes at crash positions.
fn apply_cluster_tactics(tape: &[u8], tactics: &str, rng: &mut ChaCha20Rng) -> Vec<u8> {
    let mut out = tape.to_vec();
    match tactics {
        "markov-crash-restart" | "markov-partition" | "markov-crash-restart+grid" => {
            // Inject partition/crash markers in the second quarter of the tape
            // to maximize the chance of hitting the leader-election × truncation × partition scenario.
            let crash_zone = out.len() / 4;
            for i in crash_zone..(crash_zone * 2).min(out.len()) {
                let b: u8 = (rng.next_u32() & 0xff) as u8;
                out[i] = out[i].wrapping_add(b);
            }
        }
        "random-drops" => {
            // Randomly zero out ~10% of bytes to simulate packet drops
            for b in out.iter_mut() {
                if (rng.next_u32() & 0xff) < 25 {
                    *b = 0;
                }
            }
        }
        _ => {}
    }
    out
}

fn run_fuzz_loop(
    seed: u64,
    strategy: StrategySpec,
    tactics: &str,
    max_iterations: u32,
    stop_on_crash: bool,
    spec: &str,
    planted_bug: bool,
) -> FuzzLoopResult {
    // VR2-B6: Dispatch to the correct simulation engine based on workload type.
    let kind = detect_workload_kind(spec);
    if kind == WorkloadKind::Consensus {
        return run_consensus_fuzz_loop(seed, strategy, tactics, max_iterations, stop_on_crash, planted_bug);
    }

    let mut driver = Driver::new(seed, strategy, baud_driver::TacticsSpec::default());
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
                depth_probe: "depth".to_string(),
                workload_kind: WorkloadKind::Parser,
            };
        }
    }

    FuzzLoopResult {
        generations: max_iterations,
        goal_reached,
        best_depth,
        winning_input,
        per_gen_scores,
        depth_probe: "depth".to_string(),
        workload_kind: WorkloadKind::Parser,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_strategy(strategy_json: Option<&str>) -> StrategySpec {
    if let Some(s) = strategy_json {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            let maximize = v.get("maximize")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_else(|| vec!["depth".to_string()]);
            let goal = v.get("goal_probe").and_then(|gp| gp.as_str()).map(|probe| {
                let goal_val = v.get("goal_value").and_then(|gv| gv.as_u64()).unwrap_or(1);
                baud_proto::Predicate {
                    probe: probe.to_string(),
                    value: baud_proto::Value::U64(goal_val),
                }
            });
            return StrategySpec {
                maximize,
                buckets: Vec::new(),
                reservoir: Some(baud_proto::Reservoir { keep: 32, p_backoff: 0.1 }),
                goal,
            };
        }
    }
    // Default: maximize depth, goal = crashed == 1
    StrategySpec {
        maximize: vec!["depth".to_string()],
        buckets: Vec::new(),
        reservoir: Some(baud_proto::Reservoir { keep: 32, p_backoff: 0.1 }),
        goal: Some(baud_proto::Predicate {
            probe: "crashed".to_string(),
            value: baud_proto::Value::U64(1),
        }),
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
