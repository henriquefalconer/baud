// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// POST /run/kvm — boot a guest image on the real, post-pivot KVM `Multiverse`
// (baud_multiverse::linux::Multiverse, proven deterministic on real hardware through H0-H6) and
// run it to its first halt.
//
// This is the first `baud-server` route that calls into that module at all: `/verify/determinism`
// and `/replay/:id` still construct the pre-pivot, userspace-simulation `Multiverse` from
// `baud_multiverse::lib.rs` (todo.md §14's "every existing route still imports the old pre-pivot
// Multiverse" gap, confirmed by grep before this route was added). Linux-only, like the module it
// calls (`baud_multiverse::linux` is itself `#[cfg(target_os = "linux")]`) — this workspace only
// ever builds/runs on real Linux+KVM hosts (CLAUDE.md), so there is no non-Linux fallback to write.

use axum::extract::State;
use axum::Json;
use baud_snapshot_store::SnapshotStore;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct RunKvmBody {
    /// Path to a bzImage kernel on this host's filesystem.
    pub kernel_path: String,
    /// Kernel command line. Defaults to the console-only line every fixture in this workspace uses.
    #[serde(default = "default_cmdline")]
    pub cmdline: String,
    /// The run's whole tape, hex-encoded (empty tape if omitted — a guest that never reads the
    /// tape device runs the same either way).
    #[serde(default)]
    pub tape_hex: String,
}

fn default_cmdline() -> String {
    "console=ttyS0".to_owned()
}

/// POST /run/kvm — boot `kernel_path` and run it to its first `Hlt`/`Shutdown`.
pub async fn run(Json(body): Json<RunKvmBody>) -> Json<Value> {
    let tape = match hex_decode(&body.tape_hex) {
        Some(t) => t,
        None => return Json(json!({ "error": "tape_hex must be a valid hex string" })),
    };
    let kernel_path = PathBuf::from(&body.kernel_path);
    let cmdline = body.cmdline;

    // Real ioctls (KVM_RUN and friends) block; keep them off the async executor.
    let result = tokio::task::spawn_blocking(move || boot_and_run(&kernel_path, &cmdline, tape))
        .await
        .expect("run/kvm task panicked");

    match result {
        Ok((console_output, ram_hash)) => Json(json!({
            "ok": true,
            "console_output_hex": hex_encode(&console_output),
            "ram_hash": ram_hash,
        })),
        Err(e) => Json(json!({ "error": e })),
    }
}

/// One branch/boot's result: `(console_output, ram_hash)`.
type BranchOutcome = (Vec<u8>, String);
/// `(run_id, node_id_hex)` — what a caller hands to `POST /run/kvm/resume` to fork more branches
/// from a persisted universe later.
type PersistedRef = (String, String);

fn boot_and_run(kernel_path: &Path, cmdline: &str, tape: Vec<u8>) -> Result<BranchOutcome, String> {
    let mut mv = baud_multiverse::linux::Multiverse::boot(kernel_path, cmdline, 0, 1, tape, None)
        .map_err(|e| format!("boot error: {e}"))?;
    let outcome = mv.run_to_first_halt().map_err(|e| format!("determinism hole: {e}"))?;
    Ok((outcome.console_output, outcome.ram_hash))
}

/// The work-clock constant this route uses for every boot/branch — a run-level constant
/// (`virtual_tsc = base + k * rcb`, `Multiverse::restore`'s doc), not part of captured state, so
/// every branch of the same request must share the value the branch point was booted with.
const WORK_CLOCK_K: u64 = 1;

/// Real per-branch cost is one full `KVM_CREATE_VM`/vCPU/guest-RAM-region lifecycle
/// (`Multiverse::branch`'s doc — the spec's documented small-N `fork()` fallback, not yet the
/// O(write-set) `UFFDIO_CONTINUE` sharing todo.md §14 tracks as still open), so an unbounded
/// branch count turns one HTTP request into an arbitrarily long blocking call. This caps a single
/// request at a size that stays well within normal request-timeout budgets on this dev host
/// (~200ms/branch measured by `thousand_branches_are_independent_and_deterministic`).
const MAX_BRANCHES_PER_REQUEST: usize = 256;

/// Bound for `Multiverse::run_until_branch_or_halt`'s own `max_exits` parameter, used by every
/// driver-generated branch (`run_driver_generated_branches_with_persist`). Every guest fixture this
/// route's tests use today halts within a few dozen host-side exits at most (`mark-branch-guest`'s
/// own tests bound it at 16), so this is deliberately generous headroom for a real guest, not a
/// tuned value — it exists so a guest that never calls `MARK_BRANCH` and never halts fails loud
/// (`DeterminismHole`, the same "no silent non-termination" convention `run_until_branch_or_halt`
/// itself follows) instead of blocking an HTTP request forever.
const GENERATE_BRANCH_MAX_EXITS: u32 = 65536;

#[derive(Debug, Deserialize)]
pub struct RunKvmBranchBody {
    /// Path to a bzImage kernel on this host's filesystem — booted once to establish the branch
    /// point (a snapshot taken immediately after boot, before any guest instruction runs, mirroring
    /// `thousand_branches_are_independent_and_deterministic`'s own branch point).
    pub kernel_path: String,
    /// Kernel command line. Defaults to the console-only line every fixture in this workspace uses.
    #[serde(default = "default_cmdline")]
    pub cmdline: String,
    /// One hex-encoded tape suffix per branch — each is forked independently from the shared branch
    /// point via `Multiverse::branch` and run to its first halt. Ignored (and may be omitted) when
    /// `generate` is set instead.
    #[serde(default)]
    pub branch_tapes_hex: Vec<String>,
    /// If set, persist the shared branch-point universe (RAM pages + capture body,
    /// `baud_snapshot::wire`) into the server's `SnapshotStore` under this run id before forking —
    /// the response's `persisted.node_id` can later be handed to `POST /run/kvm/resume` to fork
    /// more branches from the exact same point without re-booting the kernel at all (todo.md §14's
    /// "Not yet done: nothing persists the branch point's Universe across requests").
    #[serde(default)]
    pub persist_run_id: Option<String>,
    /// Generate branch tapes with `baud_driver::Driver` instead of the caller supplying them
    /// directly — todo.md §14's twice-flagged "natural next M-series increment" ("wiring
    /// `baud-driver`'s tape generation into this route instead of a caller-supplied fixed
    /// `tape_hex`"). Mutually exclusive with `branch_tapes_hex`.
    #[serde(default)]
    pub generate: Option<DriverGenerateSpec>,
}

/// Drives `baud_driver::Driver` to generate `count` branch tapes instead of a caller supplying
/// them literally, scoring each branch from its real drained tape-device records
/// (`observations_from_records`) and feeding the score back via `Driver::end_run` before drawing
/// the next tape — the snapshot-tree "expand a branch point, fork N continuations, score" loop
/// (todo.md §6) applied to one real branch-point request.
#[derive(Debug, Deserialize)]
pub struct DriverGenerateSpec {
    /// Seed for `baud_driver::Driver` — same seed + same guest responses reproduce byte-identical
    /// generated tapes (`Driver`'s own `same_seed_same_replies_same_tape` property).
    pub seed: u64,
    /// Number of branches to generate and run.
    pub count: usize,
    /// Bytes drawn per generated tape suffix.
    #[serde(default = "default_generate_tape_len_bytes")]
    pub tape_len_bytes: u32,
    /// Scoring strategy fed back into the driver after each branch. `maximize` picks which
    /// observed probe(s) score a run (`console_len` is always available even for guests that never
    /// touch the tape device — see `observations_from_records`); `goal`, if set, additionally marks
    /// a branch `interesting` when reached, alongside any real `Outcome::Crash` from the guest.
    #[serde(default)]
    pub strategy: baud_driver::StrategySpec,
}

fn default_generate_tape_len_bytes() -> u32 {
    4
}

/// POST /run/kvm/branch — boot `kernel_path`, snapshot immediately after boot as the shared branch
/// point, then fork one independent `Multiverse` continuation per entry in `branch_tapes_hex`
/// (`Multiverse::branch`, specs/baud-snapshot.md §4's `Snapshot::branch`) and run each to its first
/// halt. No branch observes another's state — the same guarantee
/// `thousand_branches_are_independent_and_deterministic` proves at the crate level, exposed here as
/// the M-series' first real snapshot-tree-exploration server route (todo.md §14's "Natural next
/// steps" for `/run/kvm`).
pub async fn branch(State(state): State<AppState>, Json(body): Json<RunKvmBranchBody>) -> Json<Value> {
    if body.generate.is_some() && !body.branch_tapes_hex.is_empty() {
        return Json(json!({ "error": "specify either branch_tapes_hex or generate, not both" }));
    }
    let kernel_path = PathBuf::from(&body.kernel_path);
    let cmdline = body.cmdline;
    let persist = body.persist_run_id.map(|run_id| (state.snapshot_store.clone(), run_id));

    if let Some(spec) = body.generate {
        if spec.count == 0 {
            return Json(json!({ "error": "generate.count must be at least 1" }));
        }
        if spec.count > MAX_BRANCHES_PER_REQUEST {
            return Json(json!({
                "error": format!(
                    "too many branches requested ({}) — max {MAX_BRANCHES_PER_REQUEST} per call",
                    spec.count
                )
            }));
        }
        let result = tokio::task::spawn_blocking(move || {
            let persist_ref = persist.as_ref().map(|(store, run_id)| (store.as_ref(), run_id.as_str()));
            boot_snapshot_and_generate(&kernel_path, &cmdline, spec, persist_ref)
        })
        .await
        .expect("run/kvm/branch (generate) task panicked");

        return match result {
            Ok((outcomes, summary, persisted)) => {
                let branches: Vec<Value> = outcomes.into_iter().map(generated_outcome_to_json).collect();
                let mut response = json!({
                    "ok": true,
                    "branches": branches,
                    "driver_summary": {
                        "generations": summary.generations,
                        "goal_reached": summary.goal_reached,
                        "best_tape_hex": summary.best_tape_hex,
                    },
                });
                if let Some((run_id, node_id)) = persisted {
                    response["persisted"] = json!({ "run_id": run_id, "node_id": node_id });
                }
                Json(response)
            }
            Err(e) => Json(json!({ "error": e })),
        };
    }

    if body.branch_tapes_hex.is_empty() {
        return Json(json!({ "error": "branch_tapes_hex must contain at least one tape, or set generate" }));
    }
    if body.branch_tapes_hex.len() > MAX_BRANCHES_PER_REQUEST {
        return Json(json!({
            "error": format!(
                "too many branches requested ({}) — max {MAX_BRANCHES_PER_REQUEST} per call",
                body.branch_tapes_hex.len()
            )
        }));
    }
    let mut tape_suffixes = Vec::with_capacity(body.branch_tapes_hex.len());
    for hex in &body.branch_tapes_hex {
        match hex_decode(hex) {
            Some(bytes) => tape_suffixes.push(bytes),
            None => return Json(json!({ "error": "branch_tapes_hex must contain only valid hex strings" })),
        }
    }

    let result = tokio::task::spawn_blocking(move || {
        let persist_ref = persist.as_ref().map(|(store, run_id)| (store.as_ref(), run_id.as_str()));
        boot_snapshot_and_branch(&kernel_path, &cmdline, tape_suffixes, persist_ref)
    })
    .await
    .expect("run/kvm/branch task panicked");

    match result {
        Ok((outcomes, persisted)) => {
            let branches: Vec<Value> = outcomes
                .into_iter()
                .map(|(console_output, ram_hash)| {
                    json!({ "console_output_hex": hex_encode(&console_output), "ram_hash": ram_hash })
                })
                .collect();
            let mut response = json!({ "ok": true, "branches": branches });
            if let Some((run_id, node_id)) = persisted {
                response["persisted"] = json!({ "run_id": run_id, "node_id": node_id });
            }
            Json(response)
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

/// Boot + snapshot the shared branch point, shared by every `/run/kvm/branch` flavor (fixed-tape
/// and driver-generated alike).
fn boot_and_snapshot(kernel_path: &Path, cmdline: &str) -> Result<baud_snapshot::Universe, String> {
    let mut boot = baud_multiverse::linux::Multiverse::boot(kernel_path, cmdline, 0, WORK_CLOCK_K, vec![], None)
        .map_err(|e| format!("boot error: {e}"))?;
    let mut page_store = baud_snapshot::PageStore::new();
    boot.snapshot(&mut page_store).map_err(|e| format!("snapshot error: {e}"))
}

/// Fork one independent `Multiverse::branch` continuation per tape suffix and run each to its
/// first halt. Shared by `boot_snapshot_and_branch` and `resume_and_branch`.
fn run_branches(
    universe: &baud_snapshot::Universe,
    tape_suffixes: Vec<Vec<u8>>,
) -> Result<Vec<BranchOutcome>, String> {
    let mut outcomes = Vec::with_capacity(tape_suffixes.len());
    for (i, suffix) in tape_suffixes.into_iter().enumerate() {
        let mut branch = baud_multiverse::linux::Multiverse::branch(universe, suffix, WORK_CLOCK_K, None)
            .map_err(|e| format!("branch {i} error: {e}"))?;
        let outcome = branch
            .run_to_first_halt()
            .map_err(|e| format!("branch {i} determinism hole: {e}"))?;
        outcomes.push((outcome.console_output, outcome.ram_hash));
    }
    Ok(outcomes)
}

fn boot_snapshot_and_branch(
    kernel_path: &Path,
    cmdline: &str,
    tape_suffixes: Vec<Vec<u8>>,
    persist: Option<(&SnapshotStore, &str)>,
) -> Result<(Vec<BranchOutcome>, Option<PersistedRef>), String> {
    let universe = boot_and_snapshot(kernel_path, cmdline)?;
    let persisted = match persist {
        Some((store, run_id)) => Some(persist_universe(store, run_id, &universe)?),
        None => None,
    };
    let outcomes = run_branches(&universe, tape_suffixes)?;
    Ok((outcomes, persisted))
}

/// One driver-generated branch's result: the tape `Driver::draw_bits` produced for it, its
/// outcome, the observations scored back into the driver, and whether it was "interesting"
/// (goal reached, a real guest-reported crash, or the branch stopped at a `MARK_BRANCH`
/// checkpoint rather than halting — see `mark_branch_step`'s doc). `node_id` is set only when this
/// branch was `interesting` *and* the call persisted (a `persist_run_id`'d `/run/kvm/branch`
/// request) — a real child node of the branch point a caller can hand to `POST /run/kvm/resume` or
/// `reconstruct_universe` later to re-inspect exactly this branch's final state (e.g. the guest
/// memory a crash left behind), without re-running anything.
///
/// **This now genuinely supports exploring further from a persisted branch**, for a guest that
/// calls `MARK_BRANCH` mid-execution instead of halting immediately: this function drives each
/// branch with `Multiverse::run_until_branch_or_halt` (`crates/baud-multiverse/src/linux/mod.rs`)
/// rather than `run_to_first_halt`, so a run stops the instant it hits a `MARK_BRANCH` checkpoint
/// — captured here as `mark_branch_step` — instead of only ever running a fixture to completion
/// (the "M-series sixth/seventh brick" entries' own named blocker, todo.md §14, closed at the
/// primitive level by the `mark-branch-guest` fixture and the primitive itself, and now wired into
/// this route). Every `MARK_BRANCH` stop is unconditionally treated as `interesting` (alongside
/// the pre-existing goal/crash checks) precisely because it is the one outcome, unlike a genuine
/// `Hlt`, where handing the persisted node a fresh tape suffix through `POST /run/kvm/resume`
/// actually changes what the guest does next (proven by `baud-multiverse`'s
/// `branch_from_mark_branch_checkpoint_diverges_on_new_tape_suffix`) — persisting anything less
/// would silently drop the one stop condition this feature exists to make explorable. A guest that
/// only ever halts (every fixture in this workspace before `mark-branch-guest`) behaves exactly as
/// before: `mark_branch_step` is always `None` for it, and `interesting` still depends only on
/// goal/crash. `observations_from_records` still deliberately ignores `Msg::MarkBranch` itself for
/// scoring (that has not changed and does not need to — the stop condition is orthogonal to the
/// probe-based score).
struct GeneratedBranchOutcome {
    tape_hex: String,
    console_output: Vec<u8>,
    ram_hash: String,
    observations: Vec<(String, f64)>,
    interesting: bool,
    node_id: Option<String>,
    /// `Some(step)` when this branch stopped at a `MARK_BRANCH` checkpoint (the tape cursor it
    /// stopped at) rather than running to `Hlt`; `None` for a branch that halted normally.
    mark_branch_step: Option<u64>,
}

struct DriverRunSummary {
    generations: u64,
    goal_reached: bool,
    best_tape_hex: String,
}

fn boot_snapshot_and_generate(
    kernel_path: &Path,
    cmdline: &str,
    spec: DriverGenerateSpec,
    persist: Option<(&SnapshotStore, &str)>,
) -> Result<(Vec<GeneratedBranchOutcome>, DriverRunSummary, Option<PersistedRef>), String> {
    let universe = boot_and_snapshot(kernel_path, cmdline)?;
    let persisted = match persist {
        Some((store, run_id)) => Some(persist_universe(store, run_id, &universe)?),
        None => None,
    };
    // The branch point itself was just persisted (if `persist` is set) as a fresh root node
    // (`parent: None, at_step: 0`, `persist_universe`'s own contract) — that's the parent every
    // interesting generated branch chains onto.
    let root_parent = match (&persisted, persist) {
        (Some((_, node_id_hex)), Some(_)) => {
            let id = baud_snapshot_store::NodeId::from_hex(node_id_hex)
                .map_err(|e| format!("bad persisted node_id: {e}"))?;
            Some(id)
        }
        _ => None,
    };
    let (outcomes, summary) = run_driver_generated_branches_with_persist(&universe, spec, persist, root_parent)?;
    Ok((outcomes, summary, persisted))
}

/// The snapshot-tree exploration loop (todo.md §6: "expand a branch point, fork N continuations,
/// score, keep interesting ones") applied to one shared branch point — the entry point every
/// caller that never persists (a bare `/run/kvm/branch` or any `/run/kvm/resume` generate call)
/// uses.
fn run_driver_generated_branches(
    universe: &baud_snapshot::Universe,
    spec: DriverGenerateSpec,
) -> Result<(Vec<GeneratedBranchOutcome>, DriverRunSummary), String> {
    run_driver_generated_branches_with_persist(universe, spec, None, None)
}

/// Draws a tape with `Driver::draw_bits`, fork+runs it, scores it from its drained tape-device
/// records (`observations_from_records`), and feeds the score back via `Driver::end_run` before
/// drawing the next tape. When `persist` is set, every `interesting` branch's resulting state
/// (`Multiverse::snapshot`, taken right after that branch halts) is additionally persisted as a
/// real child node of `parent` (`GeneratedBranchOutcome`'s own doc explains why this doesn't
/// support chaining a *further* generate call from it today).
fn run_driver_generated_branches_with_persist(
    universe: &baud_snapshot::Universe,
    spec: DriverGenerateSpec,
    persist: Option<(&SnapshotStore, &str)>,
    parent: Option<baud_snapshot_store::NodeId>,
) -> Result<(Vec<GeneratedBranchOutcome>, DriverRunSummary), String> {
    let mut driver = baud_driver::Driver::new(spec.seed, spec.strategy, baud_driver::TacticsSpec::default());
    let mut outcomes = Vec::with_capacity(spec.count);
    let mut goal_reached = false;
    for i in 0..spec.count {
        driver.begin_run();
        let mut suffix = Vec::with_capacity(spec.tape_len_bytes as usize);
        for _ in 0..spec.tape_len_bytes {
            suffix.push(driver.draw_bits(8)[0]);
        }
        let mut branch = baud_multiverse::linux::Multiverse::branch(universe, suffix.clone(), WORK_CLOCK_K, None)
            .map_err(|e| format!("branch {i} error: {e}"))?;
        let (run_outcome, mut records) = branch
            .run_until_branch_or_halt(GENERATE_BRANCH_MAX_EXITS)
            .map_err(|e| format!("branch {i} determinism hole: {e}"))?;
        records.extend(branch.drain_tape_records());
        let (console_output, ram_hash, mark_branch_step) = match &run_outcome {
            baud_multiverse::linux::RunUntilBranchOutcome::Halted(halt) => {
                (halt.console_output.clone(), halt.ram_hash.clone(), None)
            }
            baud_multiverse::linux::RunUntilBranchOutcome::MarkBranch { step } => {
                (branch.console_output().to_vec(), branch.ram_hash(), Some(*step))
            }
        };
        let (observations, crashed) = observations_from_records(&records, console_output.len());
        driver.end_run(&observations);
        let branch_goal = driver.is_goal_reached(&observations);
        let interesting = branch_goal || crashed || mark_branch_step.is_some();
        goal_reached |= branch_goal;

        let node_id = if interesting {
            match persist {
                Some((store, run_id)) => {
                    let mut page_store = baud_snapshot::PageStore::new();
                    let branch_universe = branch
                        .snapshot(&mut page_store)
                        .map_err(|e| format!("branch {i} snapshot error: {e}"))?;
                    // `SnapshotStore::put_universe`'s node identity is `Sha::of_node_identity(parent,
                    // at_step, tape_range)` — a function of *position*, not content (todo.md §14's
                    // "Not yet done": this store was only ever exercised with one root node per run
                    // before this feature). Every sibling branch of one generate call shares the same
                    // `parent` and the same `tape_len_bytes`, so a shared, index-independent
                    // `(0, tape_len_bytes)` `tape_range` for all of them collapses every sibling onto
                    // the *same* node id — confirmed live: a test asserting distinct node ids per
                    // branch failed until each branch's own index `i` was folded into its
                    // `tape_range`, giving every sibling a distinct, deterministic, reproducible
                    // position instead of silently overwriting one another in the store.
                    let tape_range = (
                        i as u64 * spec.tape_len_bytes as u64,
                        (i as u64 + 1) * spec.tape_len_bytes as u64,
                    );
                    let nid = persist_universe_as(store, run_id, &branch_universe, parent, tape_range.1, tape_range)?;
                    Some(nid.to_hex())
                }
                None => None,
            }
        } else {
            None
        };

        outcomes.push(GeneratedBranchOutcome {
            tape_hex: hex_encode(&suffix),
            console_output,
            ram_hash,
            observations,
            interesting,
            node_id,
            mark_branch_step,
        });
    }
    let summary = DriverRunSummary {
        generations: spec.count as u64,
        goal_reached,
        best_tape_hex: hex_encode(&driver.best_tape().tape_bytes()),
    };
    Ok((outcomes, summary))
}

/// Turns one branch's drained tape-device records (`Multiverse::drain_tape_records`,
/// `baud_proto::Msg`) into `(probe, value)` observations `Driver::end_run`/`is_goal_reached`
/// accept, plus whether the guest reported a crash (`Msg::Outcome(Outcome::Crash{..})`). Every
/// branch always carries a built-in `console_len` observation — no in-tree guest fixture emits a
/// real `Msg::Observe` probe yet (todo.md §14's tape-device entry: "no real guest ever writes to
/// this port range"), so a `strategy.maximize` pointed at `console_len` still drives real,
/// differentiated scoring today; any `Msg::Observe` records a guest does emit are additive on top
/// of it, so this keeps working unchanged once a probe-emitting guest exists.
fn observations_from_records(records: &[baud_proto::Msg], console_len: usize) -> (Vec<(String, f64)>, bool) {
    let mut observations = vec![("console_len".to_owned(), console_len as f64)];
    let mut crashed = false;
    for record in records {
        match record {
            baud_proto::Msg::Observe(obs) => {
                if let Some(value) = probe_value_as_f64(&obs.value) {
                    observations.push((obs.probe.clone(), value));
                }
            }
            baud_proto::Msg::Outcome(baud_proto::Outcome::Crash { .. }) => crashed = true,
            _ => {}
        }
    }
    (observations, crashed)
}

fn probe_value_as_f64(value: &baud_proto::Value) -> Option<f64> {
    match value {
        baud_proto::Value::U64(v) => Some(*v as f64),
        baud_proto::Value::I64(v) => Some(*v as f64),
        _ => None,
    }
}

fn generated_outcome_to_json(outcome: GeneratedBranchOutcome) -> Value {
    let mut value = json!({
        "tape_hex": outcome.tape_hex,
        "console_output_hex": hex_encode(&outcome.console_output),
        "ram_hash": outcome.ram_hash,
        "observations": outcome.observations.into_iter()
            .map(|(probe, value)| json!({ "probe": probe, "value": value }))
            .collect::<Vec<_>>(),
        "interesting": outcome.interesting,
    });
    if let Some(node_id) = outcome.node_id {
        value["node_id"] = json!(node_id);
    }
    if let Some(step) = outcome.mark_branch_step {
        value["mark_branch_step"] = json!(step);
    }
    value
}

/// Persist a captured branch-point [`baud_snapshot::Universe`] into `store` under `run_id`: every
/// *distinct* RAM page once, then the CBOR-encoded [`baud_snapshot::UniverseBody`] (RAM as page
/// hashes only, `baud_snapshot::wire`'s module doc) as a fresh root node (`parent: None` — this is
/// always the first captured point of a `persist_run_id`'d request in this route, never a deeper
/// branch of an already-persisted tree). Returns `(run_id, node_id_hex)` for the caller to hand
/// back to `POST /run/kvm/resume`.
///
/// `universe.ram_pages()` yields one entry per RAM page *slot* — `GUEST_RAM_SIZE / PAGE_SIZE`
/// (65536 for this workspace's fixed 256 MiB), not per distinct content (`baud_snapshot::wire`'s
/// module doc: "duplicate hashes are cheap to persist twice"). `SnapshotStore::put_page` already
/// skips re-encrypting a hash it has already written this store's lifetime
/// (`write_body_if_absent`), but still pays a `blake3::hash` + filesystem `stat` per call — a
/// guest whose RAM is mostly one shared zero page would otherwise redo that 65536 times per
/// persist. `seen` short-circuits every repeat within this one call, same content-addressed-dedup
/// idea `PageStore::intern` already applies one layer further in (`page_store.rs`).
fn persist_universe(
    store: &SnapshotStore,
    run_id: &str,
    universe: &baud_snapshot::Universe,
) -> Result<PersistedRef, String> {
    let node_id = persist_universe_as(store, run_id, universe, None, 0, (0, 0))?;
    Ok((run_id.to_owned(), node_id.to_hex()))
}

/// Persist any captured [`baud_snapshot::Universe`] as a node in `store`, optionally parented on an
/// already-persisted node — the general form `persist_universe` (the branch-point/root case,
/// `parent: None, at_step: 0, tape_range: (0, 0)`) and `expand_generate_level` (a deeper generated
/// branch, real `parent`/`at_step`/`tape_range`) both share. Same per-call page-dedup as
/// `persist_universe`'s own doc explains (`SnapshotStore::put_page` already skips a hash it has
/// already written this store's lifetime, but `seen` avoids even trying for a mostly-shared-RAM
/// guest's repeat page slots).
fn persist_universe_as(
    store: &SnapshotStore,
    run_id: &str,
    universe: &baud_snapshot::Universe,
    parent: Option<baud_snapshot_store::NodeId>,
    at_step: u64,
    tape_range: (u64, u64),
) -> Result<baud_snapshot_store::NodeId, String> {
    let run = baud_snapshot_store::RunId::new(run_id.to_owned());
    let mut seen = std::collections::HashSet::new();
    for (hash, bytes) in universe.ram_pages() {
        if !seen.insert(hash) {
            continue;
        }
        store.put_page(&run, bytes).map_err(|e| format!("persist page error: {e}"))?;
    }
    let encoded = baud_snapshot::encode_universe_body(&universe.to_body())
        .map_err(|e| format!("encode universe body error: {e}"))?;
    store
        .put_universe(&run, parent, at_step, tape_range, &encoded)
        .map_err(|e| format!("persist universe error: {e}"))
}

#[derive(Debug, Deserialize)]
pub struct RunKvmResumeBody {
    /// The `run_id` a prior `POST /run/kvm/branch { persist_run_id: ... }` call persisted under.
    pub run_id: String,
    /// The `node_id` that same call returned in `persisted.node_id`.
    pub node_id: String,
    /// One hex-encoded tape suffix per branch, forked from the reconstructed universe — same
    /// semantics as `RunKvmBranchBody::branch_tapes_hex`. Ignored (and may be omitted) when
    /// `generate` is set instead.
    #[serde(default)]
    pub branch_tapes_hex: Vec<String>,
    /// Generate branch tapes with `baud_driver::Driver` instead of the caller supplying them
    /// directly — the same `DriverGenerateSpec` `/run/kvm/branch` accepts, applied to a
    /// reconstructed universe instead of a freshly booted one, so an in-flight exploration can
    /// keep generating from a persisted point without re-booting the kernel. Mutually exclusive
    /// with `branch_tapes_hex`.
    #[serde(default)]
    pub generate: Option<DriverGenerateSpec>,
}

/// POST /run/kvm/resume — reconstruct a [`baud_snapshot::Universe`] previously persisted by
/// `POST /run/kvm/branch { persist_run_id: ... }` (`SnapshotStore::get_universe` +
/// `baud_snapshot::decode_universe_body` + `universe_from_body`, fetching each referenced page via
/// `SnapshotStore::get_page`), then either fork one independent `Multiverse::branch` continuation
/// per `branch_tapes_hex` entry, or drive `baud_driver::Driver` to generate `generate.count` of
/// them, and run each to its first halt — **no kernel image, no re-boot**: the reconstructed
/// universe alone is enough, closing todo.md §14's "Natural next step (1)" for `/run/kvm/branch`
/// ("accept an already-captured Universe ... as an alternative to kernel_path so a run can resume
/// instead of always cold-booting") and the twice-flagged follow-up ("`/run/kvm/resume` still only
/// accepts fixed `branch_tapes_hex`") symmetrically with `/run/kvm/branch`'s own generate mode.
pub async fn resume(State(state): State<AppState>, Json(body): Json<RunKvmResumeBody>) -> Json<Value> {
    if body.generate.is_some() && !body.branch_tapes_hex.is_empty() {
        return Json(json!({ "error": "specify either branch_tapes_hex or generate, not both" }));
    }
    let store = state.snapshot_store.clone();
    let run_id = body.run_id;
    let node_id_hex = body.node_id;

    if let Some(spec) = body.generate {
        if spec.count == 0 {
            return Json(json!({ "error": "generate.count must be at least 1" }));
        }
        if spec.count > MAX_BRANCHES_PER_REQUEST {
            return Json(json!({
                "error": format!(
                    "too many branches requested ({}) — max {MAX_BRANCHES_PER_REQUEST} per call",
                    spec.count
                )
            }));
        }
        let result = tokio::task::spawn_blocking(move || {
            let universe = reconstruct_universe(store.as_ref(), &run_id, &node_id_hex)?;
            run_driver_generated_branches(&universe, spec)
        })
        .await
        .expect("run/kvm/resume (generate) task panicked");

        return match result {
            Ok((outcomes, summary)) => {
                let branches: Vec<Value> = outcomes.into_iter().map(generated_outcome_to_json).collect();
                Json(json!({
                    "ok": true,
                    "branches": branches,
                    "driver_summary": {
                        "generations": summary.generations,
                        "goal_reached": summary.goal_reached,
                        "best_tape_hex": summary.best_tape_hex,
                    },
                }))
            }
            Err(e) => Json(json!({ "error": e })),
        };
    }

    if body.branch_tapes_hex.is_empty() {
        return Json(json!({ "error": "branch_tapes_hex must contain at least one tape, or set generate" }));
    }
    if body.branch_tapes_hex.len() > MAX_BRANCHES_PER_REQUEST {
        return Json(json!({
            "error": format!(
                "too many branches requested ({}) — max {MAX_BRANCHES_PER_REQUEST} per call",
                body.branch_tapes_hex.len()
            )
        }));
    }
    let mut tape_suffixes = Vec::with_capacity(body.branch_tapes_hex.len());
    for hex in &body.branch_tapes_hex {
        match hex_decode(hex) {
            Some(bytes) => tape_suffixes.push(bytes),
            None => return Json(json!({ "error": "branch_tapes_hex must contain only valid hex strings" })),
        }
    }

    let result = tokio::task::spawn_blocking(move || {
        resume_and_branch(store.as_ref(), &run_id, &node_id_hex, tape_suffixes)
    })
    .await
    .expect("run/kvm/resume task panicked");

    match result {
        Ok(outcomes) => {
            let branches: Vec<Value> = outcomes
                .into_iter()
                .map(|(console_output, ram_hash)| {
                    json!({ "console_output_hex": hex_encode(&console_output), "ram_hash": ram_hash })
                })
                .collect();
            Json(json!({ "ok": true, "branches": branches }))
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

/// Reconstruct a persisted [`baud_snapshot::Universe`] from the store — the shared first half of
/// both `resume_and_branch` and `/run/kvm/resume`'s generate mode.
///
/// `universe_from_body` calls its `fetch_page` closure once per RAM page *slot* in the body
/// (`persist_universe`'s doc: 65536 for this workspace's fixed 256 MiB RAM), not once per distinct
/// hash — the overwhelming majority typically repeat (e.g. one shared zero page). Each
/// `SnapshotStore::get_page` is a real disk read + age/ChaCha20Poly1305 decrypt, with no fast path
/// for a repeat like `put_page` has (`write_body_if_absent`'s "already on disk" check) — without
/// this cache a mostly-zero-filled guest would pay tens of thousands of redundant decrypts per
/// resume (found live: a real `baud-server` process spent minutes and multiple full CPU cores
/// stuck exactly here, `age::primitives::stream::Stream::decrypt_chunk`, confirmed via `gdb -p
/// <pid> -batch -ex 'thread apply all bt'` on an actually-hung manual end-to-end check — not a
/// deadlock, a real O(total pages) instead of O(distinct pages) cost).
fn reconstruct_universe(
    store: &SnapshotStore,
    run_id: &str,
    node_id_hex: &str,
) -> Result<baud_snapshot::Universe, String> {
    let run = baud_snapshot_store::RunId::new(run_id.to_owned());
    let node_id =
        baud_snapshot_store::NodeId::from_hex(node_id_hex).map_err(|e| format!("bad node_id: {e}"))?;
    let body_bytes = store.get_universe(&run, node_id).map_err(|e| format!("get_universe error: {e}"))?;
    let body =
        baud_snapshot::decode_universe_body(&body_bytes).map_err(|e| format!("decode error: {e}"))?;

    let mut page_cache: std::collections::HashMap<baud_snapshot::PageHash, Vec<u8>> =
        std::collections::HashMap::new();
    let mut page_store = baud_snapshot::PageStore::new();
    baud_snapshot::universe_from_body(body, &mut page_store, |hash| {
        if let Some(cached) = page_cache.get(&hash) {
            return Ok(cached.clone());
        }
        let sha = baud_snapshot_store::Sha::from_hex(&hash.to_hex()).map_err(|e| e.to_string())?;
        let bytes = store
            .get_page(&run, baud_snapshot_store::PageRef { address: sha })
            .map_err(|e| e.to_string())?;
        page_cache.insert(hash, bytes.clone());
        Ok(bytes)
    })
    .map_err(|e| format!("reconstruct error: {e}"))
}

fn resume_and_branch(
    store: &SnapshotStore,
    run_id: &str,
    node_id_hex: &str,
    tape_suffixes: Vec<Vec<u8>>,
) -> Result<Vec<BranchOutcome>, String> {
    let universe = reconstruct_universe(store, run_id, node_id_hex)?;
    run_branches(&universe, tape_suffixes)
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() {
        return Some(Vec::new());
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_guest_kernel_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/hello-guest/bzImage")
    }

    fn mark_branch_guest_kernel_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/mark-branch-guest/bzImage")
    }

    fn tape_echo_guest_kernel_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/tape-echo-guest/bzImage")
    }

    /// Server-level analogue of `baud-multiverse`'s own `double_boot_memory_identical`
    /// (specs/baud-multiverse.md §3.1): booting the same image+tape twice through this route's own
    /// `boot_and_run` (the exact function the HTTP handler calls, minus only the axum/JSON
    /// plumbing) must yield byte-identical console output and RAM hash. Confirms this route wires
    /// the real KVM `Multiverse` correctly, not just that the crate underneath it is deterministic.
    #[test]
    fn run_kvm_boot_is_deterministic() {
        let kernel = hello_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let (first_console, first_hash) =
            boot_and_run(&kernel, cmdline, vec![]).expect("first boot failed");
        let (second_console, second_hash) =
            boot_and_run(&kernel, cmdline, vec![]).expect("second boot failed");

        assert_eq!(first_console, second_console, "console output must be identical across two boots");
        assert_eq!(first_hash, second_hash, "RAM hash must be identical across two boots");
    }

    /// Server-level analogue of `baud-multiverse`'s own
    /// `thousand_branches_are_independent_and_deterministic`: this route's own
    /// `boot_snapshot_and_branch` (the exact function the HTTP handler calls, minus only the
    /// axum/JSON plumbing) must fork branches that don't perturb each other and that replay
    /// deterministically from the same branch point + suffix.
    #[test]
    fn run_kvm_branch_produces_independent_and_deterministic_branches() {
        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let suffixes: Vec<Vec<u8>> = (0..6u8).map(|i| vec![i, 0xAA, 0xBB, 0xCC]).collect();

        let (first_run, _) = boot_snapshot_and_branch(&kernel, cmdline, suffixes.clone(), None)
            .expect("boot_snapshot_and_branch failed");
        assert_eq!(first_run.len(), suffixes.len());
        for (i, (console_output, _ram_hash)) in first_run.iter().enumerate() {
            assert_eq!(
                console_output, &suffixes[i],
                "branch {i} must echo exactly its own tape suffix, not another branch's state"
            );
        }

        // Re-forking from a fresh branch point with the same suffixes must be byte-identical —
        // both across branches (no cross-branch bleed) and across this whole re-run (determinism).
        let (second_run, _) = boot_snapshot_and_branch(&kernel, cmdline, suffixes, None)
            .expect("second boot_snapshot_and_branch failed");
        assert_eq!(first_run, second_run, "re-forking the same suffixes must reproduce byte-identically");
    }

    fn temp_snapshot_store() -> (tempfile::TempDir, SnapshotStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity_path = dir.path().join("identity.txt");
        std::fs::write(&identity_path, baud_keys::generate_identity_file()).expect("write identity");
        let contents = std::fs::read_to_string(&identity_path).expect("read identity");
        let recipient = baud_keys::parse_public_key(&contents).expect("parse recipient");
        let store = SnapshotStore::open_with_keys(dir.path(), recipient, Some(identity_path));
        (dir, store)
    }

    /// Closes todo.md §14's "nothing persists the branch point's Universe across requests" gap
    /// end to end: `boot_snapshot_and_branch` with `persist` set must leave a universe in the
    /// store that `resume_and_branch` can reconstruct and fork from — **with no kernel image and
    /// no re-boot** — producing byte-identical branch outcomes to forking directly from the
    /// in-memory universe in the same process.
    #[test]
    fn persisted_universe_resumes_and_branches_without_reboot() {
        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let suffixes: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i, 0x11, 0x22, 0x33]).collect();

        let (_dir, store) = temp_snapshot_store();
        let run_id = "persist-test-run";

        let (direct_outcomes, persisted) =
            boot_snapshot_and_branch(&kernel, cmdline, suffixes.clone(), Some((&store, run_id)))
                .expect("boot_snapshot_and_branch with persist failed");
        let (returned_run_id, node_id_hex) = persisted.expect("persist must return a run_id/node_id");
        assert_eq!(returned_run_id, run_id);

        let resumed_outcomes = resume_and_branch(&store, run_id, &node_id_hex, suffixes)
            .expect("resume_and_branch failed");

        assert_eq!(
            direct_outcomes, resumed_outcomes,
            "resuming a persisted universe must reproduce byte-identical branch outcomes to \
             branching directly from the in-memory universe"
        );
    }

    #[test]
    fn resume_rejects_unknown_run() {
        let (_dir, store) = temp_snapshot_store();
        let err = resume_and_branch(&store, "no-such-run", &"00".repeat(32), vec![vec![1, 2, 3, 4]])
            .expect_err("resuming an unknown run must fail, not silently proceed");
        assert!(!err.is_empty());
    }

    /// Pure logic, no KVM: `observations_from_records` must extract a `(probe, value)` pair per
    /// `Msg::Observe`, always include the built-in `console_len` fallback signal, and detect a
    /// `Msg::Outcome(Outcome::Crash{..})` as `crashed`, ignoring message kinds that carry no score
    /// (`MarkBranch`/`Log`/`Eof`/`GoalReached`).
    #[test]
    fn observations_from_records_extracts_probes_and_crash() {
        let records = vec![
            baud_proto::Msg::Observe(baud_proto::Observation {
                probe: "depth".into(),
                node: 0,
                value: baud_proto::Value::U64(7),
                step: 1,
            }),
            baud_proto::Msg::MarkBranch { step: 2 },
            baud_proto::Msg::Outcome(baud_proto::Outcome::Crash {
                node: None,
                invariant: None,
                signal: None,
                detail: "planted bug".into(),
            }),
        ];
        let (observations, crashed) = observations_from_records(&records, 3);
        assert!(crashed, "a Crash outcome must be detected");
        assert!(observations.contains(&("console_len".to_owned(), 3.0)));
        assert!(observations.contains(&("depth".to_owned(), 7.0)));
        assert_eq!(observations.len(), 2, "MarkBranch must not contribute an observation");

        let (observations, crashed) = observations_from_records(&[], 5);
        assert!(!crashed);
        assert_eq!(observations, vec![("console_len".to_owned(), 5.0)]);
    }

    /// `/run/kvm/branch`'s driver-generated mode (`DriverGenerateSpec`), exercised at this route's
    /// own function level (`run_driver_generated_branches`, minus only axum/JSON plumbing) — the
    /// generate-mode analogue of `run_kvm_branch_produces_independent_and_deterministic_branches`
    /// above. Proves: (1) reproducibility — same seed + same guest replies (tape-echo-guest always
    /// echoes exactly what it's given, so the "replies" are fixed) draws byte-identical tapes
    /// across two fully independent runs (`Driver`'s own `same_seed_same_replies_same_tape`
    /// property, now exercised end-to-end against a real guest); (2) independence — every branch's
    /// console output equals exactly its own generated tape, the same no-cross-branch-bleed
    /// construction the fixed-tape test uses.
    #[test]
    fn run_kvm_branch_generate_is_reproducible_and_independent() {
        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let make_spec = || DriverGenerateSpec {
            seed: 42,
            count: 5,
            tape_len_bytes: 4,
            strategy: baud_driver::StrategySpec { maximize: vec!["console_len".into()], ..Default::default() },
        };

        let universe1 = boot_and_snapshot(&kernel, cmdline).expect("boot 1");
        let (outcomes1, summary1) =
            run_driver_generated_branches(&universe1, make_spec()).expect("generate 1");

        let universe2 = boot_and_snapshot(&kernel, cmdline).expect("boot 2");
        let (outcomes2, summary2) =
            run_driver_generated_branches(&universe2, make_spec()).expect("generate 2");

        assert_eq!(outcomes1.len(), 5);
        let tapes1: Vec<&String> = outcomes1.iter().map(|o| &o.tape_hex).collect();
        let tapes2: Vec<&String> = outcomes2.iter().map(|o| &o.tape_hex).collect();
        assert_eq!(tapes1, tapes2, "same seed must generate identical tape suffixes");
        for (o1, o2) in outcomes1.iter().zip(outcomes2.iter()) {
            assert_eq!(o1.console_output, o2.console_output, "reproducible branch outcomes");
            assert_eq!(o1.ram_hash, o2.ram_hash);
        }
        for o in &outcomes1 {
            assert_eq!(
                hex_encode(&o.console_output),
                o.tape_hex,
                "tape-echo-guest must echo exactly its own generated tape suffix"
            );
        }
        assert_eq!(summary1.generations, 5);
        assert_eq!(summary1.best_tape_hex, summary2.best_tape_hex, "reproducible driver summary");
    }

    /// A driver-generated branch point must persist and later resume exactly like a fixed-tape one
    /// — `boot_snapshot_and_generate`'s `persist` path shares `persist_universe` with
    /// `boot_snapshot_and_branch`, so this closes the same gap
    /// `persisted_universe_resumes_and_branches_without_reboot` closes, for the generate entry
    /// point.
    #[test]
    fn generated_branch_point_persists_and_resumes() {
        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let (_dir, store) = temp_snapshot_store();
        let run_id = "generate-persist-test";

        let spec = DriverGenerateSpec {
            seed: 7,
            count: 3,
            tape_len_bytes: 4,
            strategy: baud_driver::StrategySpec::default(),
        };
        let (_outcomes, _summary, persisted) =
            boot_snapshot_and_generate(&kernel, cmdline, spec, Some((&store, run_id)))
                .expect("boot_snapshot_and_generate with persist failed");
        let (returned_run_id, node_id_hex) = persisted.expect("persist must return a run_id/node_id");
        assert_eq!(returned_run_id, run_id);

        let resumed = resume_and_branch(&store, run_id, &node_id_hex, vec![vec![9, 8, 7, 6]])
            .expect("resume_and_branch failed");
        assert_eq!(
            resumed[0].0,
            vec![9, 8, 7, 6],
            "resumed branch must echo exactly its own suffix"
        );
    }

    /// `/run/kvm/resume`'s generate mode (`RunKvmResumeBody::generate`) — the symmetric follow-up
    /// todo.md §14 flagged twice ("`/run/kvm/resume` still only accepts fixed `branch_tapes_hex`").
    /// A persisted branch point must let a caller keep driving `baud_driver::Driver` generation
    /// against it with no kernel image and no re-boot, reproducing the same tapes/outcomes a
    /// direct `run_driver_generated_branches` call against the in-memory universe would.
    #[test]
    fn resumed_universe_generates_reproducible_branches() {
        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let (_dir, store) = temp_snapshot_store();
        let run_id = "generate-resume-test";
        let spec = || DriverGenerateSpec {
            seed: 99,
            count: 4,
            tape_len_bytes: 4,
            strategy: baud_driver::StrategySpec { maximize: vec!["console_len".into()], ..Default::default() },
        };

        let universe = boot_and_snapshot(&kernel, cmdline).expect("boot");
        let persisted = persist_universe(&store, run_id, &universe).expect("persist");
        let (direct_outcomes, direct_summary) =
            run_driver_generated_branches(&universe, spec()).expect("direct generate");

        let (returned_run_id, node_id_hex) = persisted;
        assert_eq!(returned_run_id, run_id);
        let reconstructed = reconstruct_universe(&store, run_id, &node_id_hex).expect("reconstruct");
        let (resumed_outcomes, resumed_summary) =
            run_driver_generated_branches(&reconstructed, spec()).expect("resumed generate");

        assert_eq!(resumed_outcomes.len(), direct_outcomes.len());
        for (r, d) in resumed_outcomes.iter().zip(direct_outcomes.iter()) {
            assert_eq!(r.tape_hex, d.tape_hex, "same seed must generate identical tape suffixes");
            assert_eq!(r.console_output, d.console_output, "resumed branch must reproduce the direct one");
            assert_eq!(r.ram_hash, d.ram_hash);
        }
        assert_eq!(resumed_summary.best_tape_hex, direct_summary.best_tape_hex);
    }

    /// Every `interesting` generated branch (not just the branch point itself) must persist as a
    /// real child node of the branch point — `node_id` set, parented correctly (`Node::parent` in
    /// the store matches the branch point's own node id, confirmed via `SnapshotStore::read_node`,
    /// not just "some node_id came back"), and independently reconstructible/resumable afterward
    /// with no kernel and no re-boot. A `goal` strategy `tape-echo-guest` satisfies on every branch
    /// (`console_len == tape_len_bytes`, since it always echoes exactly what it's given) makes every
    /// branch `interesting`, so every one of `count` branches must come back with a distinct
    /// `node_id`.
    ///
    /// This is deliberately *not* a "resume this node with a fresh tape and it explores further"
    /// test — `GeneratedBranchOutcome`'s own doc explains why that is a no-op for every guest fixture
    /// in this workspace today (each halts as soon as it consumes its given tape). What this proves
    /// instead — the property that *is* real and useful — is that a persisted interesting branch's
    /// exact final state round-trips: resuming it with *any* tape reproduces that branch's own
    /// frozen output byte-for-byte, so a caller can persist a crash and inspect that exact state
    /// later without re-running anything.
    #[test]
    fn interesting_generated_branches_persist_as_child_nodes() {
        let kernel = tape_echo_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let (_dir, store) = temp_snapshot_store();
        let run_id = "interesting-branch-persist-test";

        let spec = DriverGenerateSpec {
            seed: 5,
            count: 4,
            tape_len_bytes: 4,
            strategy: baud_driver::StrategySpec {
                goal: Some(baud_proto::Predicate {
                    probe: "console_len".into(),
                    value: baud_proto::Value::U64(4),
                }),
                ..Default::default()
            },
        };

        let (outcomes, summary, persisted) =
            boot_snapshot_and_generate(&kernel, cmdline, spec, Some((&store, run_id)))
                .expect("boot_snapshot_and_generate failed");
        let (root_run_id, root_node_id_hex) = persisted.expect("root branch point must persist");
        assert_eq!(root_run_id, run_id);
        assert!(summary.goal_reached, "tape-echo-guest always reaches the console_len goal");

        assert_eq!(outcomes.len(), 4);
        let root_node_id =
            baud_snapshot_store::NodeId::from_hex(&root_node_id_hex).expect("valid root node_id");
        let run = baud_snapshot_store::RunId::new(run_id.to_owned());
        let mut seen_node_ids = std::collections::HashSet::new();

        for outcome in &outcomes {
            assert!(outcome.interesting, "every branch must reach the console_len goal");
            let node_id_hex = outcome.node_id.as_ref().expect("interesting branch must persist a node_id");
            assert!(seen_node_ids.insert(node_id_hex.clone()), "every branch must get a distinct node_id");
            assert_ne!(node_id_hex, &root_node_id_hex, "a branch's node must differ from the branch point's");

            let node_id = baud_snapshot_store::NodeId::from_hex(node_id_hex).expect("valid node_id");
            let node = store.read_node(&run, node_id).expect("read_node failed");
            assert_eq!(
                node.parent.as_deref(),
                Some(root_node_id_hex.as_str()),
                "a generated branch's node must be parented on the branch point, not a root itself"
            );
            let _ = root_node_id; // the parent hex string above is the real assertion; keep the typed id in scope for clarity

            // Resuming this exact node — with *any* tape, since the guest is already halted —
            // must reproduce this branch's own frozen output, proving the persisted state really
            // is this specific branch's final state, not some other one's.
            let resumed = resume_and_branch(&store, run_id, node_id_hex, vec![vec![0xAB, 0xCD]])
                .expect("resuming a generated branch's node failed");
            assert_eq!(
                resumed[0].0, outcome.console_output,
                "resuming a persisted interesting branch must reproduce its own frozen output"
            );
        }
    }

    /// The concrete "next brick" todo.md §14 named after the `mark-branch-guest`/
    /// `run_until_branch_or_halt` primitives landed: a driver-generated branch that stops at a
    /// `MARK_BRANCH` checkpoint (instead of running the fixture to completion) must (1) report
    /// `mark_branch_step` and be `interesting` unconditionally, (2) persist as a real child node
    /// when `persist_run_id` is set, and (3) — the property that actually matters, unlike
    /// `interesting_generated_branches_persist_as_child_nodes`'s already-halted case — genuinely
    /// keep exploring further when resumed: handing that persisted node a *fresh* tape suffix
    /// through `resume_and_branch` must make the guest read/echo/`MARK_BRANCH` three more times
    /// with the new bytes, not replay a frozen final output (the server-route-level analogue of
    /// `baud-multiverse`'s own `branch_from_mark_branch_checkpoint_diverges_on_new_tape_suffix`).
    #[test]
    fn generated_branch_hitting_mark_branch_persists_and_resumes_further() {
        let kernel = mark_branch_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let (_dir, store) = temp_snapshot_store();
        let run_id = "mark-branch-generate-test";

        // tape_len_bytes = 1: mark-branch-guest reads one byte, echoes it, then issues
        // MARK_BRANCH — run_until_branch_or_halt stops right there, before the guest ever asks
        // for a second byte (mirrors baud-multiverse's own
        // run_until_branch_or_halt_stops_at_first_mark_branch).
        let spec = DriverGenerateSpec {
            seed: 11,
            count: 3,
            tape_len_bytes: 1,
            strategy: baud_driver::StrategySpec::default(),
        };

        let (outcomes, _summary, persisted) =
            boot_snapshot_and_generate(&kernel, cmdline, spec, Some((&store, run_id)))
                .expect("boot_snapshot_and_generate failed");
        let (root_run_id, _root_node_id_hex) = persisted.expect("root branch point must persist");
        assert_eq!(root_run_id, run_id);

        assert_eq!(outcomes.len(), 3);
        let mut seen_node_ids = std::collections::HashSet::new();
        for outcome in &outcomes {
            assert_eq!(outcome.mark_branch_step, Some(1), "must stop right after the first MARK_BRANCH");
            assert!(outcome.interesting, "a MARK_BRANCH stop must always be reported interesting");
            assert_eq!(
                outcome.console_output,
                hex_decode(&outcome.tape_hex).unwrap(),
                "console output at the checkpoint must be exactly the one byte read+echoed so far"
            );
            let node_id_hex = outcome.node_id.as_ref().expect("a MARK_BRANCH stop must persist a node_id");
            assert!(seen_node_ids.insert(node_id_hex.clone()), "every branch must get a distinct node_id");

            // Resume this exact checkpoint with fresh tape for the remaining 3 iterations
            // (mark-branch-guest loops 4 times total) and run to completion. Index 0 is never
            // re-read (the restored cursor is already past it, `Multiverse::branch`'s doc) so it
            // can be anything; indices 1..4 are the three real new bytes.
            let fresh_suffix: Vec<u8> = vec![outcome.console_output[0], 0xAA, 0xBB, 0xCC];
            let resumed = resume_and_branch(&store, run_id, node_id_hex, vec![fresh_suffix.clone()])
                .expect("resuming a MARK_BRANCH-persisted node failed");
            assert_eq!(
                resumed[0].0, fresh_suffix,
                "resuming past a MARK_BRANCH checkpoint with fresh tape must genuinely consume and \
                 echo it, not replay a frozen halt"
            );
        }
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = vec![0x00, 0xAB, 0xFF, 0x10];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
        assert_eq!(hex_decode(""), Some(Vec::new()));
        assert_eq!(hex_decode("abc"), None, "odd-length hex must be rejected");
        assert_eq!(hex_decode("zz"), None, "non-hex characters must be rejected");
    }
}
