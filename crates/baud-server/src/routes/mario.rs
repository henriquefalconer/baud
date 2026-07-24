// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /runs/mario — M8 NES Mario emulator fuzz routes
//
// Routes:
//   POST /runs/mario/fuzz      → start a Mario fuzz session
//   GET  /runs/mario/:id       → get Mario fuzz run status
//   POST /runs/:id/mario/reconstruct → reconstruct a Mario run from tape
//   POST /runs/:id/mario/verify-determinism → verify double-run equality
//
// Mario probes (from nes_bridge stdout-kv):
//   x_page, x, x_global, y, y_band, world, level, lives, game_over, game_completed
//
// Strategy (spec §8, M8):
//   maximize = ["world", "level", "x_global"]
//   buckets  = ["x_page", "y_band"]
//   goal     = "game_completed == 1"
//
// Tactics:
//   stateful-mask{p_flip=0.03}  — main run
//   random                      — negative control (should plateau quickly)

use axum::{extract::{Path, State}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::AppState;
use baud_driver::{Driver, StrategySpec};
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use rand::RngCore;

// ---------------------------------------------------------------------------
// Mario NES simulation
//
// Server-side simulation of the nes_bridge guest binary behavior.
// In a full implementation the guest binary runs under baud-multiverse in a
// sandbox; here we simulate it deterministically from the joypad tape bytes.
//
// The simulation models Mario's physics:
//   - RIGHT button: move right (+2 pixels/frame; +4 when B held for run)
//   - LEFT button:  move left (-1 pixel/frame; clamp at 0)
//   - A button:     jump (ground required; hold for higher arc)
//   - B button:     run modifier (faster horizontal movement)
//
// World/level advancement:
//   x_global / 3072 = level index (0-31; world = level_index / 4)
//   Level end at multiples of 3072 global pixels.
//
// Goal: game_completed = 1 when x_global > 8 * 4 * 3072 (past world 7-4).
// ---------------------------------------------------------------------------

// Controller button bits (NES standard)
const BTN_A:     u8 = 0x80;
const BTN_B:     u8 = 0x40;
const BTN_RIGHT: u8 = 0x01;
const BTN_LEFT:  u8 = 0x02;
const BTN_UP:    u8 = 0x08;
const BTN_DOWN:  u8 = 0x04;

const LEVEL_WIDTH: i64 = 3072; // pixels per level
const TOTAL_LEVELS: i64 = 32;  // 8 worlds × 4 levels

#[derive(Debug, Clone)]
struct MarioState {
    x_global:       i64,    // global X position
    y_pos:          i64,    // Y position (0 = top, 176 = ground)
    vy:             i64,    // vertical velocity
    on_ground:      bool,
    jump_held:      u32,
    world:          u8,     // 0-7
    level:          u8,     // 0-3
    lives:          u8,
    game_over:      bool,
    game_completed: bool,
}

impl MarioState {
    fn new() -> Self {
        MarioState {
            x_global:       0,
            y_pos:          176,  // start on ground
            vy:             0,
            on_ground:      true,
            jump_held:      0,
            world:          0,
            level:          0,
            lives:          3,
            game_over:      false,
            game_completed: false,
        }
    }

    fn step(&mut self, joypad: u8) {
        // Horizontal movement
        if joypad & BTN_RIGHT != 0 {
            let speed: i64 = if joypad & BTN_B != 0 { 4 } else { 2 };
            self.x_global += speed;
        } else if joypad & BTN_LEFT != 0 {
            self.x_global -= 1;
            if self.x_global < 0 { self.x_global = 0; }
        }

        // Vertical movement
        if joypad & BTN_A != 0 && self.on_ground {
            self.vy = -12;
            self.on_ground = false;
            self.jump_held = 1;
        } else if joypad & BTN_A != 0 && !self.on_ground && self.jump_held < 15 {
            self.vy -= 1;
            self.jump_held += 1;
        } else {
            self.jump_held = 0;
        }

        // Gravity
        if !self.on_ground {
            self.vy += 1;
            self.y_pos += self.vy;
            if self.y_pos >= 176 {
                self.y_pos = 176;
                self.vy = 0;
                self.on_ground = true;
            }
        }

        // World/level advance
        let level_idx = (self.x_global / LEVEL_WIDTH).min(TOTAL_LEVELS - 1);
        let new_world = (level_idx / 4) as u8;
        let new_level = (level_idx % 4) as u8;
        if new_world > self.world || (new_world == self.world && new_level > self.level) {
            self.world = new_world;
            self.level = new_level;
        }

        // Game completed: past world 7-4 (8 worlds × 4 levels × 3072 pixels)
        if self.x_global > TOTAL_LEVELS * LEVEL_WIDTH {
            self.game_completed = true;
        }

        // Ignore BTN_UP / BTN_DOWN / BTN_SELECT / BTN_START for this simulation
        let _ = (BTN_UP, BTN_DOWN);
    }

    fn probes(&self) -> Vec<(String, f64)> {
        let x_page   = (self.x_global >> 8) as i64;
        let x_screen = (self.x_global & 0xFF) as i64;
        let x_global = self.x_global;
        let y_band   = self.y_pos / 30;
        vec![
            ("x_page".into(),        x_page as f64),
            ("x".into(),             x_screen as f64),
            ("x_global".into(),      x_global as f64),
            ("y".into(),             self.y_pos as f64),
            ("y_band".into(),        y_band as f64),
            ("world".into(),         self.world as f64),
            ("level".into(),         self.level as f64),
            ("lives".into(),         self.lives as f64),
            ("game_over".into(),     if self.game_over { 1.0 } else { 0.0 }),
            ("game_completed".into(),if self.game_completed { 1.0 } else { 0.0 }),
        ]
    }
}

// ---------------------------------------------------------------------------
// Frame rendering (server-side — mirrors nes_bridge ppu_render_frame)
//
// Returns a 256x240 indexed8 frame buffer and its blake3 hash.
// ---------------------------------------------------------------------------

const NES_WIDTH:       usize = 256;
const NES_HEIGHT:      usize = 240;
const NES_FRAME_BYTES: usize = NES_WIDTH * NES_HEIGHT;

fn render_frame(state: &MarioState) -> Vec<u8> {
    let mut frame = vec![0x11u8; NES_FRAME_BYTES]; // sky blue

    // Ground (bottom 32 rows)
    for row in (NES_HEIGHT - 32)..NES_HEIGHT {
        for col in 0..NES_WIDTH {
            frame[row * NES_WIDTH + col] = 0x18;
        }
    }

    // Mario sprite (16×24 block at screen position)
    let mx = ((state.x_global & 0xFF) as usize).min(NES_WIDTH - 16);
    let my = (state.y_pos as usize).min(NES_HEIGHT - 24);
    for r in 0..24 {
        for c in 0..16 {
            frame[(my + r) * NES_WIDTH + mx + c] = 0x16;
        }
    }

    // World/level stripe at top
    let stripe_len = (8 + state.level as usize * 4 + state.world as usize * 8).min(NES_WIDTH);
    let stripe_color = 0x20u8
        .wrapping_add(state.world * 4)
        .wrapping_add(state.level);
    for col in 0..stripe_len {
        frame[col] = stripe_color;
    }

    frame
}

fn frame_hash(frame: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(frame).to_hex())
}


// ---------------------------------------------------------------------------
// Tape generation
// ---------------------------------------------------------------------------

fn generate_mario_tape(
    tactics: &str,
    driver: &mut Driver,
    rng: &mut ChaCha20Rng,
    best_tape: &[u8],
    n_steps: usize,
) -> Vec<u8> {
    let mut tape = Vec::with_capacity(n_steps);
    match tactics {
        "stateful-mask" => {
            // Stateful mask: start from best_tape, mutate with p_flip ≈ 0.03 per bit.
            // Each joypad byte has 8 bits; each bit flips with probability 3/100.
            // We call driver.draw_bits once to keep the corpus machinery tracking.
            let _marker = driver.draw_bits(8);
            for i in 0..n_steps {
                let base: u8 = if i < best_tape.len() { best_tape[i] } else { 0 };
                let mut byte = base;
                for bit in 0..8u8 {
                    let r = (rng.next_u32() % 100) as u8;
                    if r < 3 {
                        byte ^= 1 << bit;
                    }
                }
                tape.push(byte);
            }
        }
        "random" => {
            // White-noise random joypad bytes (negative control)
            for _ in 0..n_steps {
                tape.push(driver.draw_bits(8) as u8);
            }
        }
        _ => {
            // Default: same as random
            for _ in 0..n_steps {
                tape.push(driver.draw_bits(8) as u8);
            }
        }
    }
    tape
}

// ---------------------------------------------------------------------------
// Core fuzz loop (runs synchronously in spawn_blocking)
// ---------------------------------------------------------------------------

struct MarioFuzzResult {
    generations:     u32,
    goal_reached:    bool,
    best_x_global:   i64,
    best_world:      u8,
    best_level:      u8,
    winning_tape:    Option<String>,
    winning_frames:  Vec<String>,   // frame hashes for the winning run
    per_gen_obs:     Vec<Vec<(String, f64)>>,
    plateau_detected: bool,
}

fn run_mario_fuzz_loop(
    seed: u64,
    tactics: &str,
    max_iterations: u32,
    n_steps: usize,
    _spec: &str,
) -> MarioFuzzResult {
    let strategy = StrategySpec {
        maximize: vec!["world".to_string(), "level".to_string(), "x_global".to_string()],
        buckets: vec!["x_page".to_string(), "y_band".to_string()],
        reservoir_keep: 32,
        reservoir_p_backoff: 0.1,
        goal_probe: Some("game_completed".to_string()),
        goal_value: Some(1.0),
    };

    let mut driver = Driver::new(seed, strategy);
    let mut rng = ChaCha20Rng::seed_from_u64(seed.wrapping_add(0xdead_c0de));

    let mut per_gen_obs: Vec<Vec<(String, f64)>> = Vec::new();
    let mut best_tape: Vec<u8> = vec![0x01u8; n_steps]; // hold RIGHT by default
    let mut best_x_global: i64 = 0;
    let mut best_world: u8 = 0;
    let mut best_level: u8 = 0;
    let mut goal_reached = false;
    let mut winning_tape: Option<String> = None;
    // winning_frames tracks the best run's frame hashes; updated whenever we find a better run.
    // This is populated even if goal_reached is false (for CI stream-frames validation).
    let mut winning_frames: Vec<String> = Vec::new();

    // Track history for plateau detection
    let mut x_global_history: Vec<i64> = Vec::new();

    for gen in 0..max_iterations {
        driver.begin_run();

        let tape = generate_mario_tape(tactics, &mut driver, &mut rng, &best_tape, n_steps);

        // Simulate the NES bridge
        let mut state = MarioState::new();
        let mut frames: Vec<String> = Vec::new();
        for &joypad in &tape {
            state.step(joypad);
            let frame = render_frame(&state);
            frames.push(frame_hash(&frame));
        }

        let probes = state.probes();
        let x_global = state.x_global;

        // Update best
        let improved = x_global > best_x_global
            || state.world > best_world
            || (state.world == best_world && state.level > best_level);
        if improved {
            best_x_global = x_global;
            best_world = state.world;
            best_level = state.level;
            best_tape = tape.clone();
            // Always store the best run's frames (not just on goal)
            winning_tape = Some(hex_encode(&tape));
            winning_frames = frames.clone();
        }

        x_global_history.push(x_global);
        per_gen_obs.push(probes.clone());

        // Report to driver
        driver.end_run(&probes);

        // Check goal
        if state.game_completed {
            goal_reached = true;
            // winning_tape and winning_frames already updated in the "improved" block above.
            // Ensure they reflect this exact tape (game_completed run).
            winning_tape = Some(hex_encode(&tape));
            winning_frames = frames;
            return MarioFuzzResult {
                generations: gen + 1,
                goal_reached,
                best_x_global,
                best_world,
                best_level,
                winning_tape,
                winning_frames,
                per_gen_obs,
                plateau_detected: false,
            };
        }
    }

    // Detect plateau: last 20 generations all the same x_global max
    let plateau = detect_plateau(&x_global_history, 20);

    MarioFuzzResult {
        generations: max_iterations,
        goal_reached,
        best_x_global,
        best_world,
        best_level,
        winning_tape,
        winning_frames,
        per_gen_obs,
        plateau_detected: plateau,
    }
}

fn detect_plateau(history: &[i64], window: usize) -> bool {
    if history.len() < window * 2 { return false; }
    let n = history.len();
    let early_max = history[..n/3].iter().copied().max().unwrap_or(0);
    let late_max  = history[n - n/3..].iter().copied().max().unwrap_or(0);
    late_max <= early_max
}

// ---------------------------------------------------------------------------
// Reconstruct a Mario run from a tape
// ---------------------------------------------------------------------------

fn reconstruct_mario(tape: &[u8], max_steps: usize) -> (Vec<(String, f64)>, bool, Vec<String>) {
    let mut state = MarioState::new();
    let mut frames = Vec::new();
    let steps = tape.len().min(max_steps);
    for &joypad in &tape[..steps] {
        state.step(joypad);
        let frame = render_frame(&state);
        frames.push(frame_hash(&frame));
    }
    (state.probes(), state.game_completed, frames)
}

// ---------------------------------------------------------------------------
// Determinism verification: same tape, same seed → same frame hashes
// ---------------------------------------------------------------------------

fn verify_mario_determinism(seed: u64, n_steps: usize, tactics: &str) -> (bool, Option<usize>) {
    // Run 1
    let strategy1 = StrategySpec {
        maximize: vec!["world".into(), "level".into(), "x_global".into()],
        buckets: vec!["x_page".into(), "y_band".into()],
        reservoir_keep: 32,
        reservoir_p_backoff: 0.1,
        goal_probe: Some("game_completed".into()),
        goal_value: Some(1.0),
    };
    let mut driver1 = Driver::new(seed, strategy1.clone());
    let mut rng1 = ChaCha20Rng::seed_from_u64(seed.wrapping_add(0xdead_c0de));
    let best1 = vec![0x01u8; n_steps];
    driver1.begin_run();
    let tape1 = generate_mario_tape(tactics, &mut driver1, &mut rng1, &best1, n_steps);
    let mut state1 = MarioState::new();
    let mut hashes1 = Vec::new();
    for &j in &tape1 { state1.step(j); hashes1.push(frame_hash(&render_frame(&state1))); }

    // Run 2 (identical seed)
    let mut driver2 = Driver::new(seed, strategy1);
    let mut rng2 = ChaCha20Rng::seed_from_u64(seed.wrapping_add(0xdead_c0de));
    let best2 = vec![0x01u8; n_steps];
    driver2.begin_run();
    let tape2 = generate_mario_tape(tactics, &mut driver2, &mut rng2, &best2, n_steps);
    let mut state2 = MarioState::new();
    let mut hashes2 = Vec::new();
    for &j in &tape2 { state2.step(j); hashes2.push(frame_hash(&render_frame(&state2))); }

    // Compare
    for i in 0..hashes1.len().min(hashes2.len()) {
        if hashes1[i] != hashes2[i] {
            return (false, Some(i));
        }
    }
    if hashes1.len() != hashes2.len() {
        return (false, Some(hashes1.len().min(hashes2.len())));
    }
    (true, None)
}

// ---------------------------------------------------------------------------
// POST /runs/mario/fuzz — start a Mario fuzz session
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MarioFuzzBody {
    /// Path to spec or inline spec content
    pub spec: String,
    /// Tactics: "stateful-mask" or "random"
    #[serde(default = "default_tactics")]
    pub tactics: String,
    /// RNG seed
    #[serde(default)]
    pub seed: u64,
    /// Max iterations
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Frames per run (joypad bytes per generation)
    #[serde(default = "default_n_steps")]
    pub n_steps: usize,
}

fn default_tactics()       -> String { "stateful-mask".to_owned() }
fn default_max_iterations()-> u32    { 200 }
fn default_n_steps()       -> usize  { 300 }

pub async fn fuzz(
    State(state): State<AppState>,
    Json(body): Json<MarioFuzzBody>,
) -> Json<Value> {
    // Lint spec
    let spec_result = baud_init::lint(&body.spec);
    if let Err(e) = spec_result {
        return Json(json!({ "error": format!("spec lint error: {e}") }));
    }

    let run_id = make_id("run");
    let now = crate::state::unix_now() as i64;
    let spec_hash = blake3_hex(body.spec.as_bytes());

    // Insert run record
    let _ = sqlx::query(
        "INSERT INTO runs (id, spec_content, spec_hash, nix_ref, closure_hash, strategy, tactics, seed, budget_minutes, tape_id, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 'running', ?, ?)"
    )
    .bind(&run_id)
    .bind(&body.spec)
    .bind(&spec_hash)
    .bind("mario")
    .bind(Option::<String>::None)
    .bind("maximize=[world,level,x_global] buckets=[x_page,y_band]")
    .bind(&body.tactics)
    .bind(body.seed as i64)
    .bind(600i64)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await;

    let tactics   = body.tactics.clone();
    let seed      = body.seed;
    let max_iter  = body.max_iterations;
    let n_steps   = body.n_steps;
    let spec      = body.spec.clone();

    let result = tokio::task::spawn_blocking(move || {
        run_mario_fuzz_loop(seed, &tactics, max_iter, n_steps, &spec)
    })
    .await;

    let fuzz_result = match result {
        Ok(r)  => r,
        Err(e) => return Json(json!({ "error": format!("mario fuzz loop panic: {e}") })),
    };

    // Persist observations
    let db = state.db.clone();
    let rid = run_id.clone();
    for (step, obs) in fuzz_result.per_gen_obs.iter().enumerate() {
        let ts = crate::state::unix_now() as i64;
        for (probe, value) in obs {
            let vbytes = serde_json::to_vec(&json!(value)).unwrap_or_default();
            let _ = sqlx::query(
                "INSERT INTO observations (run_id, step, node, probe, value, recorded_at) VALUES (?, ?, 0, ?, ?, ?)"
            )
            .bind(&rid)
            .bind(step as i64)
            .bind(probe.as_str())
            .bind(&vbytes)
            .bind(ts)
            .execute(&db)
            .await;
        }
    }

    // Persist winning frame hashes as frame_records (raw blake3 bytes in BLOB column)
    for (i, hash_str) in fuzz_result.winning_frames.iter().enumerate() {
        let ts = crate::state::unix_now() as i64;
        // hash_str is "blake3:<hex>"; decode back to 32 raw bytes for the BLOB column
        let raw_bytes: Vec<u8> = if let Some(hex_part) = hash_str.strip_prefix("blake3:") {
            (0..hex_part.len())
                .step_by(2)
                .filter_map(|j| u8::from_str_radix(&hex_part[j..j+2], 16).ok())
                .collect()
        } else {
            hash_str.as_bytes().to_vec()
        };
        let _ = sqlx::query(
            "INSERT INTO frame_records (run_id, step, node, width, height, format, hash, recorded_at) VALUES (?, ?, 0, 256, 240, 'indexed8', ?, ?)"
        )
        .bind(&rid)
        .bind(i as i64)
        .bind(&raw_bytes)
        .bind(ts)
        .execute(&db)
        .await;
    }

    // Update run status
    let final_status = if fuzz_result.goal_reached { "done" } else { "done" };
    let _ = sqlx::query("UPDATE runs SET status = ?, updated_at = ? WHERE id = ?")
        .bind(final_status)
        .bind(crate::state::unix_now() as i64)
        .bind(&rid)
        .execute(&db)
        .await;

    let obs_summary: Vec<Value> = fuzz_result.per_gen_obs.iter().enumerate()
        .map(|(i, obs)| {
            let mut entry = json!({ "step": i });
            for (k, v) in obs { entry[k.as_str()] = json!(v); }
            entry
        })
        .collect();

    let exit_code: u8 = if fuzz_result.goal_reached { 2 } else { 0 };

    Json(json!({
        "run_id":          run_id,
        "tactics":         body.tactics,
        "seed":            seed,
        "generations":     fuzz_result.generations,
        "goal_reached":    fuzz_result.goal_reached,
        "best_x_global":   fuzz_result.best_x_global,
        "best_world":      fuzz_result.best_world,
        "best_level":      fuzz_result.best_level,
        "winning_tape":    fuzz_result.winning_tape,
        "winning_frames":  fuzz_result.winning_frames.len(),
        "plateau_detected":fuzz_result.plateau_detected,
        "observations":    obs_summary,
        "ok":              true,
        "exit_code":       exit_code,
    }))
}

// ---------------------------------------------------------------------------
// GET /runs/mario/:id — run status
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
            "run_id":     id,
            "status":     status,
            "tactics":    tactics,
            "seed":       seed,
            "created_at": ca,
            "workload":   "mario",
        })),
        Ok(None) => Json(json!({ "error": format!("run {id} not found") })),
        Err(e)   => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// POST /runs/:id/mario/reconstruct — reconstruct from tape
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MarioReconstructBody {
    /// Winning tape bytes (hex-encoded)
    pub tape_hex: String,
    /// Max steps
    #[serde(default = "default_reconstruct_steps")]
    pub max_steps: usize,
}
fn default_reconstruct_steps() -> usize { 300 }

pub async fn reconstruct(
    State(_state): State<AppState>,
    Path(_run_id): Path<String>,
    Json(body): Json<MarioReconstructBody>,
) -> Json<Value> {
    let tape = match hex_decode(&body.tape_hex) {
        Ok(t)  => t,
        Err(e) => return Json(json!({ "error": format!("invalid tape hex: {e}") })),
    };

    let max_steps = body.max_steps;
    let tape_len  = tape.len();
    let result = tokio::task::spawn_blocking(move || {
        reconstruct_mario(&tape, max_steps)
    })
    .await;

    match result {
        Ok((probes, completed, frames)) => Json(json!({
            "ok":              true,
            "tape_steps":      tape_len,
            "probes":          probes.iter().map(|(k,v)| json!({ "probe": k, "value": v })).collect::<Vec<_>>(),
            "game_completed":  completed,
            "frame_hashes":    frames.len(),
            "first_frame_hash": frames.first(),
        })),
        Err(e) => Json(json!({ "error": format!("reconstruction panic: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// POST /runs/:id/mario/verify-determinism — double-run equality check
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MarioVerifyBody {
    #[serde(default)]
    pub seed:    u64,
    #[serde(default = "default_n_steps")]
    pub n_steps: usize,
    #[serde(default = "default_tactics")]
    pub tactics: String,
}

pub async fn verify_determinism(
    State(_state): State<AppState>,
    Path(_run_id): Path<String>,
    Json(body): Json<MarioVerifyBody>,
) -> Json<Value> {
    let seed    = body.seed;
    let n_steps = body.n_steps;
    let tactics = body.tactics.clone();

    let result = tokio::task::spawn_blocking(move || {
        verify_mario_determinism(seed, n_steps, &tactics)
    })
    .await;

    match result {
        Ok((passed, divergent_step)) => Json(json!({
            "ok":             true,
            "passed":         passed,
            "divergent_step": divergent_step,
            "message": if passed {
                "Mario NES simulation is deterministic: identical frame hashes across two runs".to_string()
            } else {
                format!("DIVERGENCE at step {:?}", divergent_step)
            },
        })),
        Err(e) => Json(json!({ "error": format!("verify panic: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_id(prefix: &str) -> String {
    format!("{}-{}", prefix,
        uuid::Uuid::new_v4().to_string().replace('-', "").chars().take(12).collect::<String>())
}

fn blake3_hex(data: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(data).to_hex())
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 { return Err("odd length hex".to_string()); }
    (0..s.len()).step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i+2], 16).map_err(|e| e.to_string()))
        .collect()
}
