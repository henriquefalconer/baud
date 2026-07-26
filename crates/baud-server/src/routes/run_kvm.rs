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

use crate::state::unix_now;
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
    /// When set, this boot's kernel/cmdline/tape and any `Msg::Frame` records it produces are
    /// persisted under this run id (`kvm_run_meta` + `frame_records`) — the closing half of
    /// todo.md §14's eighteenth-brick gap ("a real KVM boot's frames are captured in-process but
    /// never reach the DB `baud stream frames` reads from"). Omitted/`None` keeps this route's
    /// prior stateless behaviour exactly (no DB writes at all).
    #[serde(default)]
    pub run_id: Option<String>,
    /// Path to a reproducible newc-cpio-format initramfs on this host's filesystem (e.g. the
    /// `initramfs.cpio.gz` `POST /image/build` writes) — loaded at `layout::INITRAMFS_ADDR` and
    /// pointed to by `hdr.ramdisk_image`/`ramdisk_size` (spec §4.2/§4.3,
    /// `Multiverse::boot_with_rdseed_sites`'s own `initramfs` param). Like `kernel_path`, resolved
    /// on the server host and never transferred as request content — a real initramfs can be
    /// multi-megabyte. `None` (the default) preserves this route's exact prior behavior for every
    /// hand-assembled fixture in this workspace, none of which ship a separate initramfs. Closes
    /// todo.md §14 item 1's "`baud run kvm` (`RunKvmBody`) has no `initramfs` field at all" gap.
    #[serde(default)]
    pub initramfs_path: Option<String>,
    /// A real, unmodified Linux kernel's own scheduler calibration (`calibrate_delay`) needs
    /// periodic timer interrupts to make forward progress at all — `run_to_first_halt`'s plain
    /// `KVM_RUN` loop injects nothing, so such a guest hangs forever under it
    /// (`tests/fixtures/linux-guest/BUILD.md`'s documented finding; H4's own
    /// `run_to_first_halt_with_periodic_timer` exists exactly to solve this, todo.md §14 item 1).
    /// Every hand-assembled fixture in this workspace before the real Linux guest never needed
    /// this, so it stays optional and `None` (the default) preserves this route's exact prior
    /// behavior.
    #[serde(default)]
    pub periodic_timer: Option<PeriodicTimerSpec>,
}

/// See [`RunKvmBody::periodic_timer`]'s doc for why this exists at all.
#[derive(Debug, Deserialize)]
pub struct PeriodicTimerSpec {
    /// Work-clock (retired-conditional-branch) period between ticks — spec §3.4's arm-early-then-
    /// single-step target. `guest_kernel_boots_to_userspace`'s own real-hardware-tuned value for
    /// the `linux-guest` fixture is `500_000`.
    pub period_rcb: u64,
    /// Interrupt vector to inject at each tick. Defaults to `0xec`, Linux's own
    /// `LOCAL_TIMER_VECTOR` (`arch/x86/include/asm/irq_vectors.h`) — the value every real-Linux-
    /// guest test in this workspace uses.
    #[serde(default = "default_timer_vector")]
    pub vector: u8,
    /// Bound on ticks before giving up (`DeterminismHole`, never silent non-termination) — the
    /// same convention `run_until_console_len`/`run_until_branch_or_halt` already follow.
    #[serde(default = "default_max_ticks")]
    pub max_ticks: u32,
}

fn default_timer_vector() -> u8 {
    0xec
}

fn default_max_ticks() -> u32 {
    2000
}

fn default_cmdline() -> String {
    "console=ttyS0".to_owned()
}

/// Read `path`'s bytes off the server host's filesystem, wrapping the I/O error with the path that
/// failed — the shared helper `run()` and `stream::render_frames_from_real_replay` both use to
/// resolve an optional `initramfs_path` before booting.
pub(crate) fn read_initramfs(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("failed to read initramfs_path '{path}': {e}"))
}

/// POST /run/kvm — boot `kernel_path` (plus an optional `initramfs_path`/`periodic_timer`) and run
/// it to its first `Hlt`/`Shutdown`.
pub async fn run(State(state): State<AppState>, Json(body): Json<RunKvmBody>) -> Json<Value> {
    let tape = match hex_decode(&body.tape_hex) {
        Some(t) => t,
        None => return Json(json!({ "error": "tape_hex must be a valid hex string" })),
    };
    let initramfs = match &body.initramfs_path {
        Some(path) => match read_initramfs(path) {
            Ok(bytes) => Some(bytes),
            Err(e) => return Json(json!({ "error": e })),
        },
        None => None,
    };
    let periodic_timer = body.periodic_timer.as_ref().map(|s| (s.period_rcb, s.vector, s.max_ticks));
    let kernel_path = PathBuf::from(&body.kernel_path);
    let cmdline = body.cmdline.clone();
    let tape_hex = body.tape_hex.clone();

    // Real ioctls (KVM_RUN and friends) block; keep them off the async executor.
    let result = tokio::task::spawn_blocking(move || {
        boot_run_and_drain(&kernel_path, &cmdline, tape, initramfs.as_deref(), periodic_timer)
    })
    .await
    .expect("run/kvm task panicked");

    match result {
        Ok(((console_output, ram_hash, _mark_branch_step, _node_id), records)) => {
            let mut response = json!({
                "ok": true,
                "console_output_hex": hex_encode(&console_output),
                "ram_hash": ram_hash,
            });
            if let Some(run_id) = &body.run_id {
                let params = KvmBootParams {
                    kernel_path: &body.kernel_path,
                    cmdline: &body.cmdline,
                    tape_hex: &tape_hex,
                    initramfs_path: body.initramfs_path.as_deref(),
                    periodic_timer,
                };
                match persist_kvm_run(&state, run_id, &params, &records).await {
                    Ok(frames_recorded) => response["frames_recorded"] = json!(frames_recorded),
                    Err(e) => response["persist_error"] = json!(e),
                }
            }
            Json(response)
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

/// Persist a `/run/kvm` boot's replay inputs (`kvm_run_meta`, upserted) and every `Msg::Frame`
/// record it produced (`frame_records`, hash-only — the pixel bytes stay in-process and are
/// regenerated on demand by `stream::render`'s real-replay path, `frame_records`'s own "bytes are
/// NOT stored here" convention, `migrations/0005_stream.sql`). `runs` gets an `INSERT OR IGNORE`
/// placeholder row first so this works standalone, without requiring a prior `POST /runs` call —
/// `frame_records`/`kvm_run_meta` both carry a `REFERENCES runs(id)` and sqlx's SQLite pool
/// enforces foreign keys by default.
/// The subset of a `/run/kvm` request that `kvm_run_meta` persists — bundled so a real replay
/// (`stream::render_frames_from_real_replay`) can reboot the *exact* same guest, not just the
/// same kernel+tape (todo.md §14 item 1: `initramfs_path`/`periodic_timer` widened this beyond
/// the original kernel/cmdline/tape triple, and `clippy::too_many_arguments` is why this is a
/// struct rather than five more `persist_kvm_run` parameters).
struct KvmBootParams<'a> {
    kernel_path: &'a str,
    cmdline: &'a str,
    tape_hex: &'a str,
    initramfs_path: Option<&'a str>,
    periodic_timer: Option<(u64, u8, u32)>,
}

async fn persist_kvm_run(
    state: &AppState,
    run_id: &str,
    params: &KvmBootParams<'_>,
    records: &[baud_proto::Msg],
) -> Result<usize, String> {
    let now = unix_now() as i64;
    sqlx::query(
        "INSERT OR IGNORE INTO runs (id, spec_hash, created_at, updated_at) VALUES (?, '', ?, ?)",
    )
    .bind(run_id)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| format!("persist runs row error: {e}"))?;

    let (period_rcb, vector, max_ticks) = match params.periodic_timer {
        Some((p, v, m)) => (Some(p as i64), Some(v as i64), Some(m as i64)),
        None => (None, None, None),
    };

    sqlx::query(
        "INSERT INTO kvm_run_meta (run_id, kernel_path, cmdline, tape_hex, initramfs_path, \
         periodic_timer_period_rcb, periodic_timer_vector, periodic_timer_max_ticks, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(run_id) DO UPDATE SET
            kernel_path = excluded.kernel_path,
            cmdline = excluded.cmdline,
            tape_hex = excluded.tape_hex,
            initramfs_path = excluded.initramfs_path,
            periodic_timer_period_rcb = excluded.periodic_timer_period_rcb,
            periodic_timer_vector = excluded.periodic_timer_vector,
            periodic_timer_max_ticks = excluded.periodic_timer_max_ticks,
            created_at = excluded.created_at",
    )
    .bind(run_id)
    .bind(params.kernel_path)
    .bind(params.cmdline)
    .bind(params.tape_hex)
    .bind(params.initramfs_path)
    .bind(period_rcb)
    .bind(vector)
    .bind(max_ticks)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| format!("persist kvm_run_meta error: {e}"))?;

    let mut frames_recorded = 0usize;
    for record in records {
        if let baud_proto::Msg::Frame(frame) = record {
            sqlx::query(
                "INSERT INTO frame_records (run_id, node, step, width, height, format, hash, recorded_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(run_id)
            .bind(frame.node as i64)
            .bind(frame.step as i64)
            .bind(frame.width as i64)
            .bind(frame.height as i64)
            .bind(pixfmt_str(&frame.format))
            .bind(frame.hash.0.to_vec())
            .bind(now)
            .execute(&state.db)
            .await
            .map_err(|e| format!("persist frame_records error: {e}"))?;
            frames_recorded += 1;
        }
    }
    Ok(frames_recorded)
}

/// The `frame_records.format` string convention already used by `stream::append_frame`'s callers
/// (`migrations/0005_stream.sql`'s own comment: `"rgba8888" | "rgb565" | "indexed8"`).
fn pixfmt_str(format: &baud_proto::PixFmt) -> &'static str {
    match format {
        baud_proto::PixFmt::Rgba8888 => "rgba8888",
        baud_proto::PixFmt::Rgb565 => "rgb565",
        baud_proto::PixFmt::Indexed8 => "indexed8",
    }
}

/// One branch/boot's result: `(console_output, ram_hash, mark_branch_step, node_id)`.
/// `mark_branch_step` is `Some(step)` when the branch stopped at a `MARK_BRANCH` checkpoint (the
/// tape cursor it stopped at) instead of running to `Hlt` — see
/// [`GeneratedBranchOutcome::mark_branch_step`]'s doc for why this distinction matters. `node_id` is
/// set only when the branch stopped at `MARK_BRANCH` *and* the call persisted (`boot_snapshot_and_
/// branch`'s `persist` argument, or `resume_and_branch`, which always persists since it already has
/// a store/run_id) — the fixed-tape analogue of `GeneratedBranchOutcome::node_id`, a real child node
/// a caller can hand back to `POST /run/kvm/resume` to keep exploring past this exact checkpoint.
/// `boot_and_run` (a plain boot-to-first-halt, never a branch fork) always leaves both `None`.
type BranchOutcome = (Vec<u8>, String, Option<u64>, Option<String>);
/// `(run_id, node_id_hex)` — what a caller hands to `POST /run/kvm/resume` to fork more branches
/// from a persisted universe later.
type PersistedRef = (String, String);
/// Every drained tape-device record (`Msg::Frame` included) a call to [`run_branches`] produced,
/// one `Vec` per branch, parallel to (not folded into) its `Vec<BranchOutcome>` — kept as a
/// separate parallel `Vec` rather than `Vec<(BranchOutcome, Vec<Msg>)>` because `baud_proto::Msg`
/// is not `PartialEq`, and several existing tests compare a whole `Vec<BranchOutcome>` via
/// `assert_eq!` (`run_branches`'s own doc explains this in full).
type BranchRecords = Vec<Vec<baud_proto::Msg>>;

#[cfg(test)]
fn boot_and_run(kernel_path: &Path, cmdline: &str, tape: Vec<u8>) -> Result<BranchOutcome, String> {
    boot_run_and_drain(kernel_path, cmdline, tape, None, None).map(|(outcome, _records)| outcome)
}

/// Boot `kernel_path` (plus an optional `initramfs`/`periodic_timer`, see [`RunKvmBody`]'s doc for
/// why both exist), run to first `Hlt`/`Shutdown`, then drain every tape-device record the guest
/// emitted along the way (`Multiverse::drain_tape_records`) — the same boot `boot_and_run` does,
/// plus the drain `/run/kvm`'s `run()` handler needs to persist real `Msg::Frame` records
/// (previously captured in-process and immediately dropped, todo.md §14's eighteenth-brick gap).
fn boot_run_and_drain(
    kernel_path: &Path,
    cmdline: &str,
    tape: Vec<u8>,
    initramfs: Option<&[u8]>,
    periodic_timer: Option<(u64, u8, u32)>,
) -> Result<(BranchOutcome, Vec<baud_proto::Msg>), String> {
    let rdseed_sites = crate::rdseed_sites::load_rdseed_sites(kernel_path)?;
    let mut mv = baud_multiverse::linux::Multiverse::boot_with_rdseed_sites(
        kernel_path,
        cmdline,
        0,
        1,
        tape,
        None,
        initramfs,
        rdseed_sites,
    )
    .map_err(|e| format!("boot error: {e}"))?;
    let halt = match periodic_timer {
        Some((period_rcb, vector, max_ticks)) => {
            let (_ticks, halt) = mv
                .run_to_first_halt_with_periodic_timer(period_rcb, vector, max_ticks)
                .map_err(|e| format!("determinism hole: {e}"))?;
            halt
        }
        None => mv.run_to_first_halt().map_err(|e| format!("determinism hole: {e}"))?,
    };
    let records = mv.drain_tape_records();
    Ok(((halt.console_output, halt.ram_hash, None, None), records))
}

/// Re-boot a real KVM guest and return only the `Msg::Frame` records it produced, in order — the
/// primitive `stream::render`'s real-replay path uses to regenerate actual pixels for a run that
/// `/run/kvm` persisted (`kvm_run_meta`), instead of fabricating a synthetic gradient from a
/// stored hash.
pub(crate) fn boot_and_drain_frames(
    kernel_path: &Path,
    cmdline: &str,
    tape: Vec<u8>,
    initramfs: Option<&[u8]>,
    periodic_timer: Option<(u64, u8, u32)>,
) -> Result<Vec<baud_proto::FrameRecord>, String> {
    let (_outcome, records) = boot_run_and_drain(kernel_path, cmdline, tape, initramfs, periodic_timer)?;
    Ok(records
        .into_iter()
        .filter_map(|m| match m {
            baud_proto::Msg::Frame(frame) => Some(frame),
            _ => None,
        })
        .collect())
}

/// The work-clock constant this route uses for every boot/branch — a run-level constant
/// (`virtual_tsc = base + k * rcb`, `Multiverse::restore`'s doc), not part of captured state, so
/// every branch of the same request must share the value the branch point was booted with.
pub(crate) const WORK_CLOCK_K: u64 = 1;

/// Real per-branch cost is one full `KVM_CREATE_VM`/vCPU/guest-RAM-region lifecycle
/// (`Multiverse::branch`'s doc — the spec's documented small-N `fork()` fallback, not yet the
/// O(write-set) `UFFDIO_CONTINUE` sharing todo.md §14 tracks as still open), so an unbounded
/// branch count turns one HTTP request into an arbitrarily long blocking call. This caps a single
/// request at a size that stays well within normal request-timeout budgets on this dev host
/// (~200ms/branch measured by `thousand_branches_are_independent_and_deterministic`).
const MAX_BRANCHES_PER_REQUEST: usize = 256;

/// Bound for `Multiverse::run_until_branch_or_halt`'s own `max_exits` parameter, used by every
/// branch this route forks — both a driver-generated branch
/// (`run_driver_generated_branches_with_persist`) and a fixed-tape `branch_tapes_hex` one
/// (`run_branches`). Every guest fixture this route's tests use today halts within a few dozen
/// host-side exits at most (`mark-branch-guest`'s own tests bound it at 16), so this is
/// deliberately generous headroom for a real guest, not a tuned value — it exists so a guest that
/// never calls `MARK_BRANCH` and never halts fails loud (`DeterminismHole`, the same "no silent
/// non-termination" convention `run_until_branch_or_halt` itself follows) instead of blocking an
/// HTTP request forever.
const BRANCH_MAX_EXITS: u32 = 65536;

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
    /// Same field as [`RunKvmBody::initramfs_path`], applied to the boot that establishes this
    /// call's shared branch point — a real Linux kernel guest needs its initramfs to reach `/init`
    /// regardless of whether it's booted once (`/run/kvm`) or as a branch point (`/run/kvm/branch`).
    /// `None` (the default) preserves this route's exact prior behavior for every hand-assembled
    /// fixture in this workspace. Closes todo.md §14 item 1's "`/run/kvm/branch` and `/run/kvm/
    /// resume` ... still do not accept either" gap for this route's half of it.
    #[serde(default)]
    pub initramfs_path: Option<String>,
    /// Same field as [`RunKvmBody::periodic_timer`], applied to every branch this call forks (not
    /// the boot that establishes the branch point itself — `boot_and_snapshot` never runs the
    /// guest at all, it snapshots immediately after boot). A real Linux kernel guest's
    /// `calibrate_delay()` needs periodic ticks to make forward progress past the branch point at
    /// all, exactly as `RunKvmBody::periodic_timer`'s doc explains for a plain boot. `None` (the
    /// default) preserves this route's exact prior behavior.
    #[serde(default)]
    pub periodic_timer: Option<PeriodicTimerSpec>,
    /// One optional run id per entry in `branch_tapes_hex` (same length, or omitted entirely) —
    /// when `Some`, that branch's replay inputs and every `Msg::Frame` record it produced are
    /// persisted into `kvm_run_meta`/`frame_records` under this run id via the same
    /// `persist_kvm_run` `/run/kvm { run_id }` already uses, closing todo.md §14's "a real
    /// Linux-guest run persisted via branch/resume still won't get a `kvm_run_meta` row" gap for
    /// this route's fixed-tape mode. This works because `boot_and_snapshot` always snapshots the
    /// branch point with an **empty** tape, before any guest instruction runs (see its own doc) —
    /// so a branch's own suffix *is* its entire replay tape from a cold boot, byte-identical to
    /// forking from the snapshot. `stream::render`'s real-replay path can then reboot this branch's
    /// exact guest (`kernel_path`/`cmdline`/`initramfs_path`/`periodic_timer`, all from this same
    /// request, plus `tape_hex = branch_tapes_hex[i]`) to regenerate real pixels instead of falling
    /// back to the synthetic-gradient path. Ignored when `generate` is set (see
    /// `DriverGenerateSpec::frame_run_id_prefix` instead).
    #[serde(default)]
    pub frame_run_ids: Vec<Option<String>>,
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
    /// When set, every generated branch's replay inputs and `Msg::Frame` records are persisted
    /// under the run id `"{prefix}-{i}"` (`i` = the branch's index in this call, `0`-based) —
    /// the generate-mode analogue of `RunKvmBranchBody::frame_run_ids` (see its doc for why a
    /// branch's own tape is its whole replay tape from a cold boot). A caller can't name each
    /// generated branch's run id ahead of time the way `frame_run_ids` names a `branch_tapes_hex`
    /// entry, since the driver — not the caller — chooses each branch's tape; the index-derived
    /// name is the simplest scheme that still gives every branch a distinct, reproducible id.
    /// **Only honored by `/run/kvm/branch`'s generate mode** — `/run/kvm/resume` reuses this same
    /// spec type for its own generate mode, but resume never has a `kernel_path`/`cmdline` to
    /// reboot from (it reconstructs a `Universe` from the store, not from a kernel image), so there
    /// is nothing a real-replay reboot could target; `resume` rejects a request that sets this
    /// field rather than silently ignoring it.
    #[serde(default)]
    pub frame_run_id_prefix: Option<String>,
}

fn default_generate_tape_len_bytes() -> u32 {
    4
}

/// Persist one branch's frames under `run_id` via [`persist_kvm_run`] and return the JSON fragment
/// `branch()` folds into its response's `frame_persistence` array — shared by the fixed-tape
/// (`RunKvmBranchBody::frame_run_ids`) and generate (`DriverGenerateSpec::frame_run_id_prefix`)
/// modes, mirroring `run()`'s own single-boot `persist_kvm_run` call.
async fn persist_branch_frames(
    state: &AppState,
    run_id: &str,
    params: &KvmBootParams<'_>,
    records: &[baud_proto::Msg],
) -> Value {
    match persist_kvm_run(state, run_id, params, records).await {
        Ok(frames_recorded) => json!({ "run_id": run_id, "frames_recorded": frames_recorded }),
        Err(e) => json!({ "run_id": run_id, "persist_error": e }),
    }
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
    let cmdline = body.cmdline.clone();
    let persist = body.persist_run_id.map(|run_id| (state.snapshot_store.clone(), run_id));
    let initramfs = match &body.initramfs_path {
        Some(path) => match read_initramfs(path) {
            Ok(bytes) => Some(bytes),
            Err(e) => return Json(json!({ "error": e })),
        },
        None => None,
    };
    let periodic_timer = body.periodic_timer.as_ref().map(|s| (s.period_rcb, s.vector, s.max_ticks));

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
        let frame_run_id_prefix = spec.frame_run_id_prefix.clone();
        let result = tokio::task::spawn_blocking(move || {
            let persist_ref = persist.as_ref().map(|(store, run_id)| (store.as_ref(), run_id.as_str()));
            boot_snapshot_and_generate(
                &kernel_path,
                &cmdline,
                spec,
                persist_ref,
                initramfs.as_deref(),
                periodic_timer,
            )
        })
        .await
        .expect("run/kvm/branch (generate) task panicked");

        return match result {
            Ok((outcomes, summary, persisted)) => {
                let mut branches = Vec::with_capacity(outcomes.len());
                let mut frame_persistence = Vec::new();
                for (i, outcome) in outcomes.into_iter().enumerate() {
                    if let Some(prefix) = &frame_run_id_prefix {
                        let run_id = format!("{prefix}-{i}");
                        let params = KvmBootParams {
                            kernel_path: &body.kernel_path,
                            cmdline: &body.cmdline,
                            tape_hex: &outcome.tape_hex,
                            initramfs_path: body.initramfs_path.as_deref(),
                            periodic_timer,
                        };
                        frame_persistence.push(
                            persist_branch_frames(&state, &run_id, &params, &outcome.records).await,
                        );
                    }
                    branches.push(generated_outcome_to_json(outcome));
                }
                let mut response = json!({
                    "ok": true,
                    "branches": branches,
                    "driver_summary": {
                        "generations": summary.generations,
                        "goal_reached": summary.goal_reached,
                        "best_tape_hex": summary.best_tape_hex,
                        "cumulative_generation": summary.cumulative_generation,
                    },
                });
                if let Some((run_id, node_id)) = persisted {
                    response["persisted"] = json!({ "run_id": run_id, "node_id": node_id });
                }
                if !frame_persistence.is_empty() {
                    response["frame_persistence"] = json!(frame_persistence);
                }
                Json(response)
            }
            Err(e) => Json(json!({ "error": e })),
        };
    }

    // An empty `branch_tapes_hex` is only a caller error when there is nothing else useful for
    // this call to do; with `persist_run_id` set it is instead "persist-only" mode — boot,
    // snapshot, and persist the branch point without forking any continuations from it, so a
    // later `POST /shell-into/{run_id}/{node_id}` (or `/run/kvm/resume`) has a node to resume
    // into for a guest with no `MARK_BRANCH` checkpoint of its own (e.g. an interactive-console
    // fixture like `shell-guest` that never halts and never calls `MARK_BRANCH`, so it has no
    // other way to reach the store via this route). `boot_snapshot_and_branch` already handles an
    // empty `tape_suffixes` correctly (`run_branches` returns `Ok(vec![])` for zero suffixes) —
    // only this HTTP-level guard needed relaxing.
    if body.branch_tapes_hex.is_empty() && persist.is_none() {
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
    if !body.frame_run_ids.is_empty() && body.frame_run_ids.len() != body.branch_tapes_hex.len() {
        return Json(json!({
            "error": format!(
                "frame_run_ids length ({}) must match branch_tapes_hex length ({})",
                body.frame_run_ids.len(),
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
        boot_snapshot_and_branch(
            &kernel_path,
            &cmdline,
            tape_suffixes,
            persist_ref,
            initramfs.as_deref(),
            periodic_timer,
        )
    })
    .await
    .expect("run/kvm/branch task panicked");

    match result {
        Ok((outcomes, records, persisted)) => {
            let mut branches = Vec::with_capacity(outcomes.len());
            let mut frame_persistence = Vec::new();
            for (i, (outcome, branch_records)) in outcomes.into_iter().zip(records).enumerate() {
                if let Some(Some(run_id)) = body.frame_run_ids.get(i) {
                    let params = KvmBootParams {
                        kernel_path: &body.kernel_path,
                        cmdline: &body.cmdline,
                        tape_hex: &body.branch_tapes_hex[i],
                        initramfs_path: body.initramfs_path.as_deref(),
                        periodic_timer,
                    };
                    frame_persistence
                        .push(persist_branch_frames(&state, run_id, &params, &branch_records).await);
                }
                branches.push(branch_outcome_to_json(outcome));
            }
            let mut response = json!({ "ok": true, "branches": branches });
            if let Some((run_id, node_id)) = persisted {
                response["persisted"] = json!({ "run_id": run_id, "node_id": node_id });
            }
            if !frame_persistence.is_empty() {
                response["frame_persistence"] = json!(frame_persistence);
            }
            Json(response)
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

/// `BranchOutcome` → JSON, shared by `/run/kvm/branch` and `/run/kvm/resume`'s fixed-tape
/// (`branch_tapes_hex`) response bodies. Mirrors `generated_outcome_to_json`'s `mark_branch_step`
/// handling for the generate-mode response.
fn branch_outcome_to_json((console_output, ram_hash, mark_branch_step, node_id): BranchOutcome) -> Value {
    let mut value = json!({ "console_output_hex": hex_encode(&console_output), "ram_hash": ram_hash });
    if let Some(step) = mark_branch_step {
        value["mark_branch_step"] = json!(step);
    }
    if let Some(node_id) = node_id {
        value["node_id"] = json!(node_id);
    }
    value
}

/// Boot + snapshot the shared branch point, shared by every `/run/kvm/branch` flavor (fixed-tape
/// and driver-generated alike). `initramfs` mirrors `RunKvmBody::initramfs_path` — a real Linux
/// kernel guest needs it to reach `/init` before there is anything meaningful to snapshot.
fn boot_and_snapshot(
    kernel_path: &Path,
    cmdline: &str,
    initramfs: Option<&[u8]>,
) -> Result<baud_snapshot::Universe, String> {
    let rdseed_sites = crate::rdseed_sites::load_rdseed_sites(kernel_path)?;
    let mut boot = baud_multiverse::linux::Multiverse::boot_with_rdseed_sites(
        kernel_path,
        cmdline,
        0,
        WORK_CLOCK_K,
        vec![],
        None,
        initramfs,
        rdseed_sites,
    )
    .map_err(|e| format!("boot error: {e}"))?;
    let mut page_store = baud_snapshot::PageStore::new();
    boot.snapshot(&mut page_store).map_err(|e| format!("snapshot error: {e}"))
}

/// Fork one independent `Multiverse::branch` continuation per tape suffix and run each until it
/// either halts or hits a `MARK_BRANCH` checkpoint (`Multiverse::run_until_branch_or_halt`, the
/// fixed-tape analogue of `run_driver_generated_branches_with_persist`'s own use of the same
/// primitive). Shared by `boot_snapshot_and_branch` (`POST /run/kvm/branch`'s fixed-tape mode) and
/// `resume_and_branch` (`POST /run/kvm/resume`'s fixed-tape mode).
///
/// Before this, both callers ran every branch with `run_to_first_halt`, so a caller resuming a
/// `MARK_BRANCH`-persisted checkpoint (persisted via the generate path's `persist_universe_as`) had
/// to supply a tape suffix long enough to carry the guest all the way to its next real `Hlt` — there
/// was no way to ask a `branch_tapes_hex` fork/resume to itself stop at the guest's *next*
/// `MARK_BRANCH` (todo.md §14's own "Not yet done" for this function). Now it stops there instead,
/// reporting `mark_branch_step` just like the generate path does, so a caller can advance a
/// checkpoint one `MARK_BRANCH` at a time with `branch_tapes_hex` too.
///
/// When `persist` is set, every branch that stops at `MARK_BRANCH` is additionally persisted as a
/// real child node of `parent` (`GeneratedBranchOutcome`'s own "unconditionally interesting" doc
/// applies here too — a `MARK_BRANCH` stop, unlike a genuine `Hlt`, is the one outcome where handing
/// the persisted node a fresh suffix through `POST /run/kvm/resume` actually changes what the guest
/// does next), closing the "can detect but cannot persist-and-resume-further" gap the fixed-tape
/// path used to have relative to the generate path (todo.md §14's ninth-brick entry).
/// Returns each branch's outcome alongside every tape-device record it produced (`Msg::Frame`
/// included) — a parallel `Vec<Vec<Msg>>`, not folded into `BranchOutcome` itself, so every existing
/// caller/test that compares/derives `BranchOutcome`/`Vec<BranchOutcome>` (`baud_proto::Msg` is not
/// `PartialEq`) keeps working unchanged. `boot_snapshot_and_branch`'s HTTP caller
/// (`routes::run_kvm::branch`) uses the records to persist a branch's frames under
/// `RunKvmBranchBody::frame_run_ids`, mirroring `run()`'s own `persist_kvm_run` call.
fn run_branches(
    universe: &baud_snapshot::Universe,
    tape_suffixes: Vec<Vec<u8>>,
    persist: Option<(&SnapshotStore, &str, Option<baud_snapshot_store::NodeId>)>,
    periodic_timer: Option<(u64, u8, u32)>,
) -> Result<(Vec<BranchOutcome>, BranchRecords), String> {
    let mut outcomes = Vec::with_capacity(tape_suffixes.len());
    let mut all_records = Vec::with_capacity(tape_suffixes.len());
    let mut offset: u64 = 0;
    for (i, suffix) in tape_suffixes.into_iter().enumerate() {
        let suffix_len = suffix.len() as u64;
        let mut branch = baud_multiverse::linux::Multiverse::branch(universe, suffix, WORK_CLOCK_K, None)
            .map_err(|e| format!("branch {i} error: {e}"))?;
        let (run_outcome, mut records) = match periodic_timer {
            Some((period_rcb, vector, max_ticks)) => {
                let (_ticks, outcome, records) = branch
                    .run_until_branch_or_halt_with_periodic_timer(period_rcb, vector, max_ticks)
                    .map_err(|e| format!("branch {i} determinism hole: {e}"))?;
                (outcome, records)
            }
            None => branch
                .run_until_branch_or_halt(BRANCH_MAX_EXITS)
                .map_err(|e| format!("branch {i} determinism hole: {e}"))?,
        };
        records.extend(branch.drain_tape_records());
        let (console_output, ram_hash, mark_branch_step) = match &run_outcome {
            baud_multiverse::linux::RunUntilBranchOutcome::Halted(halt) => {
                (halt.console_output.clone(), halt.ram_hash.clone(), None)
            }
            baud_multiverse::linux::RunUntilBranchOutcome::MarkBranch { step } => {
                (branch.console_output().to_vec(), branch.ram_hash(), Some(*step))
            }
        };
        let tape_range = (offset, offset + suffix_len);
        offset = tape_range.1;
        let node_id = if mark_branch_step.is_some() {
            match persist {
                Some((store, run_id, parent)) => {
                    let mut page_store = baud_snapshot::PageStore::new();
                    let branch_universe = branch
                        .snapshot(&mut page_store)
                        .map_err(|e| format!("branch {i} snapshot error: {e}"))?;
                    let nid =
                        persist_universe_as(store, run_id, &branch_universe, parent, tape_range.1, tape_range)?;
                    Some(nid.to_hex())
                }
                None => None,
            }
        } else {
            None
        };
        outcomes.push((console_output, ram_hash, mark_branch_step, node_id));
        all_records.push(records);
    }
    Ok((outcomes, all_records))
}

/// `persisted`'s node id, parsed, for use as the `parent` of any deeper node persisted in the same
/// call — `None` when nothing was persisted at all (no `run_id` to persist deeper nodes under
/// either). Shared by `boot_snapshot_and_branch` and `boot_snapshot_and_generate`.
fn persisted_root_parent(
    persisted: &Option<PersistedRef>,
) -> Result<Option<baud_snapshot_store::NodeId>, String> {
    match persisted {
        Some((_, node_id_hex)) => {
            let id = baud_snapshot_store::NodeId::from_hex(node_id_hex)
                .map_err(|e| format!("bad persisted node_id: {e}"))?;
            Ok(Some(id))
        }
        None => Ok(None),
    }
}

fn boot_snapshot_and_branch(
    kernel_path: &Path,
    cmdline: &str,
    tape_suffixes: Vec<Vec<u8>>,
    persist: Option<(&SnapshotStore, &str)>,
    initramfs: Option<&[u8]>,
    periodic_timer: Option<(u64, u8, u32)>,
) -> Result<(Vec<BranchOutcome>, BranchRecords, Option<PersistedRef>), String> {
    let universe = boot_and_snapshot(kernel_path, cmdline, initramfs)?;
    let persisted = match persist {
        Some((store, run_id)) => Some(persist_universe(store, run_id, &universe)?),
        None => None,
    };
    let parent = persisted_root_parent(&persisted)?;
    let branch_persist = persist.map(|(store, run_id)| (store, run_id, parent));
    let (outcomes, records) = run_branches(&universe, tape_suffixes, branch_persist, periodic_timer)?;
    Ok((outcomes, records, persisted))
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
    /// Every tape-device record this branch produced (`Msg::Frame` included) — not exposed by
    /// `generated_outcome_to_json` (the HTTP response stays exactly as before), only consumed by
    /// `branch()`'s `frame_run_id_prefix` handling to persist a branch's frames the same way
    /// `run()`'s `persist_kvm_run` does for a plain `/run/kvm` boot.
    records: Vec<baud_proto::Msg>,
}

struct DriverRunSummary {
    generations: u64,
    goal_reached: bool,
    best_tape_hex: String,
    /// `Driver::generation()` after this call — the *cumulative* counter (carries over from a
    /// prior call's persisted `DriverState` when `persist` is set), distinct from `generations`
    /// above (how many generations *this* call ran). Lets an HTTP caller confirm driver-state
    /// persistence actually accumulated across requests instead of resetting every time (e.g.
    /// `drive/m9.sh`'s M9.6b).
    cumulative_generation: u64,
}

fn boot_snapshot_and_generate(
    kernel_path: &Path,
    cmdline: &str,
    spec: DriverGenerateSpec,
    persist: Option<(&SnapshotStore, &str)>,
    initramfs: Option<&[u8]>,
    periodic_timer: Option<(u64, u8, u32)>,
) -> Result<(Vec<GeneratedBranchOutcome>, DriverRunSummary, Option<PersistedRef>), String> {
    let universe = boot_and_snapshot(kernel_path, cmdline, initramfs)?;
    let persisted = match persist {
        Some((store, run_id)) => Some(persist_universe(store, run_id, &universe)?),
        None => None,
    };
    // The branch point itself was just persisted (if `persist` is set) as a fresh root node
    // (`parent: None, at_step: 0`, `persist_universe`'s own contract) — that's the parent every
    // interesting generated branch chains onto.
    let root_parent = persisted_root_parent(&persisted)?;
    let (outcomes, summary) =
        run_driver_generated_branches_with_persist(&universe, spec, persist, root_parent, periodic_timer)?;
    Ok((outcomes, summary, persisted))
}

/// The snapshot-tree exploration loop (todo.md §6: "expand a branch point, fork N continuations,
/// score, keep interesting ones") applied to one shared branch point, with persistence off — no
/// production route calls this directly any more (`/run/kvm/branch`'s bare, unpersisted case goes
/// through `run_driver_generated_branches_with_persist(.., None, None)` inline, and
/// `/run/kvm/resume`'s generate mode always persists via `resume_and_generate`), so this is now a
/// test-only convenience for exercising the no-persist path directly against an in-memory universe.
#[cfg(test)]
fn run_driver_generated_branches(
    universe: &baud_snapshot::Universe,
    spec: DriverGenerateSpec,
) -> Result<(Vec<GeneratedBranchOutcome>, DriverRunSummary), String> {
    run_driver_generated_branches_with_persist(universe, spec, None, None, None)
}

/// Draws a tape with `Driver::draw_bits`, fork+runs it, scores it from its drained tape-device
/// records (`observations_from_records`), and feeds the score back via `Driver::end_run` before
/// drawing the next tape. When `persist` is set, every `interesting` branch's resulting state
/// (`Multiverse::snapshot`, taken right after that branch halts) is additionally persisted as a
/// real child node of `parent` (`GeneratedBranchOutcome`'s own doc explains why this doesn't
/// support chaining a *further* generate call from it today).
///
/// When `persist` is set, the `Driver`'s own exploration state (`best`/`reservoir`/`generation`/
/// rng stream position — `baud_driver::DriverState`) is loaded from the store before the first
/// generation and written back after the last, closing todo.md §14's "Driver state persistence
/// across requests" gap: before this, every call — including a `resume`d one continuing an
/// already-persisted branch point — built `Driver::new` from scratch, so a second generate call
/// against the same `run_id` re-explored with an empty `best`/`reservoir` and `generation` reset
/// to 0, discarding everything the first call learned. `spec.seed`/`spec.strategy` still come
/// from the request every time (a resumed call can change strategy mid-exploration); only the
/// accumulated progress persists.
fn run_driver_generated_branches_with_persist(
    universe: &baud_snapshot::Universe,
    spec: DriverGenerateSpec,
    persist: Option<(&SnapshotStore, &str)>,
    parent: Option<baud_snapshot_store::NodeId>,
    periodic_timer: Option<(u64, u8, u32)>,
) -> Result<(Vec<GeneratedBranchOutcome>, DriverRunSummary), String> {
    let mut driver = baud_driver::Driver::new(spec.seed, spec.strategy, baud_driver::TacticsSpec::default());
    if let Some((store, run_id)) = persist {
        let run = baud_snapshot_store::RunId::new(run_id.to_owned());
        if store.has_driver_state(&run) {
            let bytes = store.get_driver_state(&run).map_err(|e| format!("get_driver_state error: {e}"))?;
            let state: baud_driver::DriverState =
                serde_json::from_slice(&bytes).map_err(|e| format!("decode driver state error: {e}"))?;
            driver.apply_state(state);
        }
    }
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
        let (run_outcome, mut records) = match periodic_timer {
            Some((period_rcb, vector, max_ticks)) => {
                let (_ticks, outcome, records) = branch
                    .run_until_branch_or_halt_with_periodic_timer(period_rcb, vector, max_ticks)
                    .map_err(|e| format!("branch {i} determinism hole: {e}"))?;
                (outcome, records)
            }
            None => branch
                .run_until_branch_or_halt(BRANCH_MAX_EXITS)
                .map_err(|e| format!("branch {i} determinism hole: {e}"))?,
        };
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
            records,
        });
    }
    let summary = DriverRunSummary {
        generations: spec.count as u64,
        goal_reached,
        best_tape_hex: hex_encode(&driver.best_tape().tape_bytes()),
        cumulative_generation: driver.generation(),
    };
    if let Some((store, run_id)) = persist {
        let run = baud_snapshot_store::RunId::new(run_id.to_owned());
        let encoded = serde_json::to_vec(&driver.export_state())
            .map_err(|e| format!("encode driver state error: {e}"))?;
        store.put_driver_state(&run, &encoded).map_err(|e| format!("put_driver_state error: {e}"))?;
    }
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
    /// Same field as [`RunKvmBody::periodic_timer`], applied to every branch this call forks from
    /// the reconstructed universe. No `initramfs_path` here: `resume` never boots a kernel at all
    /// (`reconstruct_universe` rebuilds the `Multiverse` from a persisted `Universe`, not from
    /// `kernel_path`), but a resumed real-Linux-guest checkpoint still needs periodic ticks to make
    /// forward progress past it, exactly like a fresh branch does — closes todo.md §14 item 1's
    /// "`/run/kvm/resume` ... still do not accept either" gap for this route's half of it.
    #[serde(default)]
    pub periodic_timer: Option<PeriodicTimerSpec>,
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
    let periodic_timer = body.periodic_timer.as_ref().map(|s| (s.period_rcb, s.vector, s.max_ticks));

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
        // `frame_run_id_prefix` needs a `kernel_path`/`cmdline` to reboot from for a real-replay
        // frame persist — `resume` never has one (it reconstructs a `Universe` from the store, not
        // from a kernel image, see `RunKvmResumeBody`'s own doc), so honoring it here would either
        // silently no-op or persist wrong replay inputs. Reject loudly instead
        // (`DriverGenerateSpec::frame_run_id_prefix`'s own doc names this exact restriction).
        if spec.frame_run_id_prefix.is_some() {
            return Json(json!({
                "error": "generate.frame_run_id_prefix is not supported by /run/kvm/resume \
                          (resume has no kernel_path/cmdline to reboot from for real-replay frame \
                          persistence) — use /run/kvm/branch instead"
            }));
        }
        let result = tokio::task::spawn_blocking(move || {
            resume_and_generate(store.as_ref(), &run_id, &node_id_hex, spec, periodic_timer)
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
                        "cumulative_generation": summary.cumulative_generation,
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
        resume_and_branch(store.as_ref(), &run_id, &node_id_hex, tape_suffixes, periodic_timer)
    })
    .await
    .expect("run/kvm/resume task panicked");

    match result {
        // `_records`: resume has no kernel_path/cmdline to reboot a real replay from, so there is
        // nothing to persist these into yet — see `resume_and_branch`'s widened return type doc.
        Ok((outcomes, _records)) => {
            let branches: Vec<Value> = outcomes.into_iter().map(branch_outcome_to_json).collect();
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
pub(crate) fn reconstruct_universe(
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

/// Resuming from a persisted node always has a `store`/`run_id` to persist deeper into (unlike
/// `boot_snapshot_and_branch`, where persistence is opt-in via `persist_run_id`) — so a fresh
/// `MARK_BRANCH` stop reached from here is always persisted as a child of the resumed node, letting
/// a caller walk a `branch_tapes_hex` chain of checkpoints via repeated `POST /run/kvm/resume` calls
/// without ever switching to `generate` mode.
fn resume_and_branch(
    store: &SnapshotStore,
    run_id: &str,
    node_id_hex: &str,
    tape_suffixes: Vec<Vec<u8>>,
    periodic_timer: Option<(u64, u8, u32)>,
) -> Result<(Vec<BranchOutcome>, BranchRecords), String> {
    let universe = reconstruct_universe(store, run_id, node_id_hex)?;
    let parent = baud_snapshot_store::NodeId::from_hex(node_id_hex).map_err(|e| format!("bad node_id: {e}"))?;
    run_branches(&universe, tape_suffixes, Some((store, run_id, Some(parent))), periodic_timer)
}

/// Generate-mode analogue of `resume_and_branch`: resuming from a persisted node always has a
/// `store`/`run_id` to persist deeper into, exactly as `resume_and_branch`'s own doc explains for
/// the fixed-tape path — so this mirrors it instead of `run_driver_generated_branches`'s bare,
/// non-persisting form (`boot_snapshot_and_generate`'s opt-in `persist_run_id`, which makes sense
/// for a *fresh* `/run/kvm/branch` boot, does not apply here: a resumed node already has a home to
/// persist into, so there is nothing to opt into). Closes the real gap `/run/kvm/resume`'s generate
/// mode used to have: every interesting branch it found was silently dropped on the floor, unlike
/// its own fixed-tape sibling and unlike `/run/kvm/branch`'s own generate mode when persisted.
fn resume_and_generate(
    store: &SnapshotStore,
    run_id: &str,
    node_id_hex: &str,
    spec: DriverGenerateSpec,
    periodic_timer: Option<(u64, u8, u32)>,
) -> Result<(Vec<GeneratedBranchOutcome>, DriverRunSummary), String> {
    let universe = reconstruct_universe(store, run_id, node_id_hex)?;
    let parent = baud_snapshot_store::NodeId::from_hex(node_id_hex).map_err(|e| format!("bad node_id: {e}"))?;
    run_driver_generated_branches_with_persist(&universe, spec, Some((store, run_id)), Some(parent), periodic_timer)
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

    fn framebuffer_guest_kernel_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/framebuffer-guest/bzImage")
    }

    /// Server-level analogue of `baud-multiverse`'s own
    /// `framebuffer_guest_frame_is_reproducible_across_boots`, exercised through this route's own
    /// `boot_and_drain_frames` — the exact primitive `stream::render`'s real-replay path
    /// (`render_frames_from_real_replay`) calls — instead of `Multiverse` directly. Confirms the
    /// server-level wrapper preserves both the frame's real pixel bytes and their determinism
    /// across two boots, and that `baud_stream::to_rgba` (what `render_frames_from_real_replay`
    /// feeds the encoder) converts them exactly as `framebuffer-guest`'s own fixture doc expects.
    #[test]
    fn boot_and_drain_frames_is_deterministic_and_carries_real_pixels() {
        let kernel = framebuffer_guest_kernel_path();
        let cmdline = "console=ttyS0";

        let first = boot_and_drain_frames(&kernel, cmdline, vec![], None, None).expect("first boot failed");
        let second = boot_and_drain_frames(&kernel, cmdline, vec![], None, None).expect("second boot failed");

        assert_eq!(first.len(), 1, "framebuffer-guest emits exactly one Frame record: {first:?}");
        assert_eq!(second.len(), 1, "framebuffer-guest emits exactly one Frame record: {second:?}");

        let frame = &first[0];
        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 2);
        assert_eq!(frame.format, baud_proto::PixFmt::Indexed8);
        assert_eq!(frame.bytes.as_deref(), Some([10u8, 20, 30, 40].as_slice()));
        assert_eq!(first[0].hash, second[0].hash, "frame hash must be identical across two boots");
        assert_eq!(first[0].bytes, second[0].bytes, "raw pixel bytes must be identical across two boots");

        // What `render_frames_from_real_replay` actually feeds the Y4M/QOI encoder: real,
        // guest-produced pixels converted with baud-stream's own format conversion — not a
        // synthetic hash-seeded gradient.
        let rgba = baud_stream::to_rgba(frame.bytes.as_ref().unwrap(), &frame.format);
        assert_eq!(rgba.len(), 2 * 2 * 4, "2x2 Indexed8 frame must expand to 16 RGBA bytes");
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

        let (first_console, first_hash, _, _) =
            boot_and_run(&kernel, cmdline, vec![]).expect("first boot failed");
        let (second_console, second_hash, _, _) =
            boot_and_run(&kernel, cmdline, vec![]).expect("second boot failed");

        assert_eq!(first_console, second_console, "console output must be identical across two boots");
        assert_eq!(first_hash, second_hash, "RAM hash must be identical across two boots");
    }

    fn linux_guest_kernel_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/linux-guest/bzImage")
    }

    fn linux_guest_initramfs() -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/linux-guest/initramfs.cpio.gz");
        std::fs::read(path).expect("read linux-guest initramfs fixture")
    }

    /// Closes todo.md §14 item 1's "`baud run kvm` (`RunKvmBody`) has no `initramfs` field at
    /// all, so a `baud image build`-produced image cannot yet be booted through the CLI/server
    /// path end-to-end" gap: boots the exact real, unmodified Linux 6.18 kernel + initramfs
    /// `baud_multiverse::linux::guest_kernel_boots_to_userspace` already proves reach `/init` on
    /// real `/dev/kvm`, but this time through `boot_run_and_drain` — the precise function `POST
    /// /run/kvm`'s HTTP handler calls, minus only the axum/JSON plumbing — with a real
    /// `initramfs_path` and `periodic_timer` threaded through exactly as an HTTP caller would
    /// supply them. Without both, this hangs forever (no periodic ticks) or never finds `/init`
    /// (no initramfs) — this test is the server-route-level proof that the wiring gap is closed,
    /// not just the underlying `baud-multiverse` primitive.
    #[test]
    fn run_kvm_boots_a_real_linux_guest_with_initramfs_and_periodic_timer() {
        let kernel = linux_guest_kernel_path();
        let initramfs = linux_guest_initramfs();
        let cmdline = baud_multiverse::linux::bootparams::DETERMINISTIC_CMDLINE;
        const PERIOD_RCB: u64 = 500_000;
        const TIMER_VECTOR: u8 = 0xec;
        const MAX_TICKS: u32 = 2000;

        let ((console_output, _ram_hash, _mark_branch_step, _node_id), _records) = boot_run_and_drain(
            &kernel,
            cmdline,
            vec![],
            Some(&initramfs),
            Some((PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)),
        )
        .expect("real linux-guest boot through boot_run_and_drain failed");

        let console = String::from_utf8_lossy(&console_output);
        assert!(
            console.contains("baud-guest: minimal kernel reached /init"),
            "guest must reach /init and print its marker; got:\n{console}"
        );
    }

    fn linux_guest_checkpoint_initramfs() -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../baud-multiverse/tests/fixtures/linux-guest/checkpoint_initramfs.cpio.gz");
        std::fs::read(path).expect("read linux-guest checkpoint initramfs fixture")
    }

    /// Closes todo.md §14 item 1's other named gap: `/run/kvm/branch` (and, by the same
    /// `boot_and_snapshot`/`run_branches` primitives, `/run/kvm/resume`) previously had no
    /// `initramfs_path`/`periodic_timer` fields at all, so a real Linux kernel guest could be
    /// booted once through `/run/kvm` but never explored via branch/resume — this is the
    /// server-route-level proof that gap is closed, mirroring
    /// `run_kvm_boots_a_real_linux_guest_with_initramfs_and_periodic_timer`'s own doc but for the
    /// branch path. Uses the same real, unmodified Linux 6.18 kernel as that test, plus the
    /// `checkpoint_initramfs.cpio.gz` variant (`tests/fixtures/linux-guest/BUILD.md`) whose `/init`
    /// issues one `MARK_BRANCH` right before powering off — `baud-multiverse`'s own
    /// `double_boot_ram_hash_identical` test proves this same fixture reaches that checkpoint via
    /// `run_until_branch_or_halt_with_periodic_timer` directly; this proves the *route*
    /// (`boot_snapshot_and_branch` → `run_branches`) threads `initramfs`/`periodic_timer` into that
    /// same primitive correctly, not just that the primitive itself works. Without either field,
    /// this branch would hang forever (no periodic ticks, `calibrate_delay()` never returns) or
    /// never reach `/init` (no initramfs) inside `BRANCH_MAX_EXITS`-bounded `KVM_RUN` exits — a
    /// world apart from a real kernel's boot cost, so it would time out loudly rather than pass by
    /// accident.
    #[test]
    fn run_kvm_branch_boots_a_real_linux_guest_with_initramfs_and_periodic_timer() {
        let kernel = linux_guest_kernel_path();
        let initramfs = linux_guest_checkpoint_initramfs();
        let cmdline = baud_multiverse::linux::bootparams::DETERMINISTIC_CMDLINE;
        const PERIOD_RCB: u64 = 500_000;
        const TIMER_VECTOR: u8 = 0xec;
        const MAX_TICKS: u32 = 2000;

        let (outcomes, _records, _persisted) = boot_snapshot_and_branch(
            &kernel,
            cmdline,
            vec![vec![]],
            None,
            Some(&initramfs),
            Some((PERIOD_RCB, TIMER_VECTOR, MAX_TICKS)),
        )
        .expect("boot_snapshot_and_branch with a real linux-guest initramfs+periodic_timer failed");

        assert_eq!(outcomes.len(), 1);
        let (_console_output, _ram_hash, mark_branch_step, _node_id) = &outcomes[0];
        assert!(
            mark_branch_step.is_some(),
            "the checkpoint fixture's /init must stop this branch at its MARK_BRANCH checkpoint, \
             not halt or hang"
        );
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

        let (first_run, _records, _) =
            boot_snapshot_and_branch(&kernel, cmdline, suffixes.clone(), None, None, None)
                .expect("boot_snapshot_and_branch failed");
        assert_eq!(first_run.len(), suffixes.len());
        for (i, (console_output, _ram_hash, mark_branch_step, _node_id)) in first_run.iter().enumerate() {
            assert_eq!(
                console_output, &suffixes[i],
                "branch {i} must echo exactly its own tape suffix, not another branch's state"
            );
            assert_eq!(*mark_branch_step, None, "tape-echo-guest never calls MARK_BRANCH, only halts");
        }

        // Re-forking from a fresh branch point with the same suffixes must be byte-identical —
        // both across branches (no cross-branch bleed) and across this whole re-run (determinism).
        let (second_run, _records, _) = boot_snapshot_and_branch(&kernel, cmdline, suffixes, None, None, None)
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

        let (direct_outcomes, _direct_records, persisted) =
            boot_snapshot_and_branch(&kernel, cmdline, suffixes.clone(), Some((&store, run_id)), None, None)
                .expect("boot_snapshot_and_branch with persist failed");
        let (returned_run_id, node_id_hex) = persisted.expect("persist must return a run_id/node_id");
        assert_eq!(returned_run_id, run_id);

        let (resumed_outcomes, _resumed_records) = resume_and_branch(&store, run_id, &node_id_hex, suffixes, None)
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
        let err = resume_and_branch(&store, "no-such-run", &"00".repeat(32), vec![vec![1, 2, 3, 4]], None)
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
            frame_run_id_prefix: None,
        };

        let universe1 = boot_and_snapshot(&kernel, cmdline, None).expect("boot 1");
        let (outcomes1, summary1) =
            run_driver_generated_branches(&universe1, make_spec()).expect("generate 1");

        let universe2 = boot_and_snapshot(&kernel, cmdline, None).expect("boot 2");
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
            frame_run_id_prefix: None,
        };
        let (_outcomes, _summary, persisted) =
            boot_snapshot_and_generate(&kernel, cmdline, spec, Some((&store, run_id)), None, None)
                .expect("boot_snapshot_and_generate with persist failed");
        let (returned_run_id, node_id_hex) = persisted.expect("persist must return a run_id/node_id");
        assert_eq!(returned_run_id, run_id);

        let (resumed, _records) = resume_and_branch(&store, run_id, &node_id_hex, vec![vec![9, 8, 7, 6]], None)
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
            frame_run_id_prefix: None,
        };

        let universe = boot_and_snapshot(&kernel, cmdline, None).expect("boot");
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
            frame_run_id_prefix: None,
        };

        let (outcomes, summary, persisted) =
            boot_snapshot_and_generate(&kernel, cmdline, spec, Some((&store, run_id)), None, None)
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
            let (resumed, _records) = resume_and_branch(&store, run_id, node_id_hex, vec![vec![0xAB, 0xCD]], None)
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
    /// through `resume_and_branch` must make the guest read/echo the new byte and land on the
    /// guest's *next* `MARK_BRANCH`, not replay a frozen final output (the server-route-level
    /// analogue of `baud-multiverse`'s own
    /// `branch_from_mark_branch_checkpoint_diverges_on_new_tape_suffix`).
    ///
    /// `resume_and_branch` used to always call `run_to_first_halt`, so resuming here required a tape
    /// long enough to carry the guest all the way to its final `Hlt`; now that it too calls
    /// `run_until_branch_or_halt` (this iteration's own fix, mirroring what the seventh/eighth
    /// bricks did for the driver-generated path), it correctly stops at the guest's *second*
    /// `MARK_BRANCH` instead — the fixed-tape analogue of `two_level_mark_branch_checkpoints_chain`
    /// (`crates/baud-multiverse/src/linux/mod.rs`) proven here at the HTTP-route level.
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
            frame_run_id_prefix: None,
        };

        let (outcomes, _summary, persisted) =
            boot_snapshot_and_generate(&kernel, cmdline, spec, Some((&store, run_id)), None, None)
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

            // Resume this exact checkpoint with fresh tape for the guest's next iteration. Index 0
            // is never re-read (the restored cursor is already past it, `Multiverse::branch`'s
            // doc) so it can be anything; index 1 is the one real new byte for the guest's second
            // loop iteration — `run_until_branch_or_halt` stops the instant it hits the guest's own
            // next MARK_BRANCH, so extra trailing bytes beyond that aren't needed or consumed.
            let fresh_suffix: Vec<u8> = vec![outcome.console_output[0], 0xAA];
            let (resumed, _records) =
                resume_and_branch(&store, run_id, node_id_hex, vec![fresh_suffix.clone()], None)
                    .expect("resuming a MARK_BRANCH-persisted node failed");
            let (resumed_console, _resumed_ram_hash, resumed_mark_branch_step, _resumed_node_id) = &resumed[0];
            assert_eq!(
                *resumed_mark_branch_step,
                Some(2),
                "resuming a branch_tapes_hex fork of a MARK_BRANCH-persisted node must stop at the \
                 guest's next MARK_BRANCH, not silently require a full tape to Hlt"
            );
            assert_eq!(
                resumed_console, &fresh_suffix,
                "resuming past a MARK_BRANCH checkpoint with fresh tape must genuinely consume and \
                 echo it, not replay a frozen halt"
            );
        }
    }

    /// The fixed-tape (`branch_tapes_hex`) sibling of
    /// `generated_branch_hitting_mark_branch_persists_and_resumes_further` — todo.md §14's ninth-
    /// brick entry's own named next step: a `boot_snapshot_and_branch`/`resume_and_branch` fork that
    /// stops at `MARK_BRANCH` must not just *report* `mark_branch_step` but also persist a real
    /// child node (parented on the branch point, confirmed via `SnapshotStore::read_node`, the same
    /// check `interesting_generated_branches_persist_as_child_nodes` does for the generate path),
    /// and that node must genuinely support resuming exploration further — a fresh tape suffix
    /// handed to `resume_and_branch` must make the guest consume/echo it and land on its *next*
    /// `MARK_BRANCH`, not replay frozen state.
    #[test]
    fn fixed_tape_branch_hitting_mark_branch_persists_and_resumes_further() {
        let kernel = mark_branch_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let (_dir, store) = temp_snapshot_store();
        let run_id = "mark-branch-fixed-tape-test";

        // mark-branch-guest reads one byte, echoes it, then issues MARK_BRANCH —
        // run_until_branch_or_halt stops right there, same one-byte suffix the generate-mode
        // sibling test above uses (tape_len_bytes: 1).
        let (outcomes, _records, persisted) =
            boot_snapshot_and_branch(&kernel, cmdline, vec![vec![0x42]], Some((&store, run_id)), None, None)
                .expect("boot_snapshot_and_branch failed");
        let (root_run_id, root_node_id_hex) = persisted.expect("root branch point must persist");
        assert_eq!(root_run_id, run_id);

        assert_eq!(outcomes.len(), 1);
        let (console_output, _ram_hash, mark_branch_step, node_id) = &outcomes[0];
        assert_eq!(*mark_branch_step, Some(1), "must stop right after the first MARK_BRANCH");
        assert_eq!(
            console_output, &vec![0x42],
            "console output at the checkpoint must be exactly the one byte read+echoed so far"
        );
        let node_id_hex = node_id.as_ref().expect("a MARK_BRANCH stop must persist a node_id");
        assert_ne!(node_id_hex, &root_node_id_hex, "a branch's node must differ from the branch point's");

        let run = baud_snapshot_store::RunId::new(run_id.to_owned());
        let node = store
            .read_node(&run, baud_snapshot_store::NodeId::from_hex(node_id_hex).expect("valid node_id"))
            .expect("read_node failed");
        assert_eq!(
            node.parent.as_deref(),
            Some(root_node_id_hex.as_str()),
            "a fixed-tape branch's persisted node must be parented on the branch point"
        );

        // Same technique as the generate-mode sibling test: index 0 is never re-read (the restored
        // cursor is already past it) so it can be anything; index 1 is the one real new byte for
        // the guest's second loop iteration.
        let fresh_suffix: Vec<u8> = vec![console_output[0], 0xAA];
        let (resumed, _records) = resume_and_branch(&store, run_id, node_id_hex, vec![fresh_suffix.clone()], None)
            .expect("resuming a MARK_BRANCH-persisted node failed");
        let (resumed_console, _resumed_ram_hash, resumed_mark_branch_step, resumed_node_id) = &resumed[0];
        assert_eq!(
            *resumed_mark_branch_step,
            Some(2),
            "resuming a branch_tapes_hex fork of a MARK_BRANCH-persisted node must stop at the \
             guest's next MARK_BRANCH, not silently require a full tape to Hlt"
        );
        assert_eq!(
            resumed_console, &fresh_suffix,
            "resuming past a MARK_BRANCH checkpoint with fresh tape must genuinely consume and \
             echo it, not replay a frozen halt"
        );
        let resumed_node_id_hex =
            resumed_node_id.as_ref().expect("resume_and_branch must also persist a further MARK_BRANCH stop");
        assert_ne!(
            resumed_node_id_hex, node_id_hex,
            "the second checkpoint's node must differ from the first's"
        );
        let resumed_node = store
            .read_node(&run, baud_snapshot_store::NodeId::from_hex(resumed_node_id_hex).expect("valid node_id"))
            .expect("read_node failed");
        assert_eq!(
            resumed_node.parent.as_deref(),
            Some(node_id_hex.as_str()),
            "resuming must chain the new node onto the resumed-from node, not the original branch point"
        );
    }

    /// The generate-mode analogue of `fixed_tape_branch_hitting_mark_branch_persists_and_resumes_further`,
    /// proving the real gap `resume_and_generate` closes: before this fix, `/run/kvm/resume`'s
    /// generate mode called the bare `run_driver_generated_branches` (no `persist`/`parent`
    /// arguments at all), so every interesting branch it found — even a `MARK_BRANCH` stop, always
    /// `interesting` unconditionally — was silently dropped instead of persisted, unlike its own
    /// fixed-tape sibling `resume_and_branch` (persists unconditionally, no opt-in needed) and
    /// unlike `/run/kvm/branch`'s own generate mode when `persist_run_id` is set. Asserts every
    /// `MARK_BRANCH` stop reached via `resume_and_generate` gets a real, distinct `node_id`,
    /// correctly parented on the *resumed-from* node (not the original root), via
    /// `SnapshotStore::read_node` — the same check the fixed-tape sibling test performs.
    #[test]
    fn resumed_generate_persists_mark_branch_children() {
        let kernel = mark_branch_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let (_dir, store) = temp_snapshot_store();
        let run_id = "mark-branch-resume-generate-test";

        // First branch point: boot + persist (no generate here — just get a root node to resume
        // from), mirroring boot_snapshot_and_branch's own root-then-children shape.
        let (outcomes, _records, persisted) =
            boot_snapshot_and_branch(&kernel, cmdline, vec![vec![0x42]], Some((&store, run_id)), None, None)
                .expect("boot_snapshot_and_branch failed");
        let (root_run_id, _root_node_id_hex) = persisted.expect("root branch point must persist");
        assert_eq!(root_run_id, run_id);
        let (_console_output, _ram_hash, mark_branch_step, node_id) = &outcomes[0];
        assert_eq!(*mark_branch_step, Some(1));
        let node_id_hex = node_id.as_ref().expect("first MARK_BRANCH stop must persist").clone();

        // Resume that checkpoint in generate mode — this is the path that used to drop everything.
        let spec = DriverGenerateSpec {
            seed: 21,
            count: 3,
            tape_len_bytes: 2,
            strategy: baud_driver::StrategySpec::default(),
            frame_run_id_prefix: None,
        };
        let (outcomes, _summary) = resume_and_generate(&store, run_id, &node_id_hex, spec, None)
            .expect("resume_and_generate failed");

        assert_eq!(outcomes.len(), 3);
        let run = baud_snapshot_store::RunId::new(run_id.to_owned());
        let mut seen_node_ids = std::collections::HashSet::new();
        for outcome in &outcomes {
            assert_eq!(outcome.mark_branch_step, Some(2), "must stop at the guest's next MARK_BRANCH");
            assert!(outcome.interesting, "a MARK_BRANCH stop must always be reported interesting");
            let child_node_id_hex = outcome
                .node_id
                .as_ref()
                .expect("resume_and_generate must persist every MARK_BRANCH stop, not drop it");
            assert!(seen_node_ids.insert(child_node_id_hex.clone()), "every branch must get a distinct node_id");
            let child_node = store
                .read_node(&run, baud_snapshot_store::NodeId::from_hex(child_node_id_hex).expect("valid node_id"))
                .expect("read_node failed");
            assert_eq!(
                child_node.parent.as_deref(),
                Some(node_id_hex.as_str()),
                "a resumed generate branch must be parented on the resumed-from node, not the original root"
            );
        }
    }

    /// Regression test for todo.md §14's "Driver state persistence across requests" gap: two
    /// sequential `resume_and_generate` calls against the same `run_id`/`node_id`/seed must
    /// accumulate one `Driver`'s `generation`/`reservoir`, not each start a fresh `Driver` that
    /// discards what the previous call learned. Before the fix, the second call's persisted
    /// `DriverState` would show `generation == spec.count` (reset), not `2 * spec.count`.
    #[test]
    fn resume_and_generate_persists_and_resumes_driver_state_across_calls() {
        let kernel = mark_branch_guest_kernel_path();
        let cmdline = "console=ttyS0";
        let (_dir, store) = temp_snapshot_store();
        let run_id = "driver-state-resume-test";

        let (outcomes, _records, persisted) =
            boot_snapshot_and_branch(&kernel, cmdline, vec![vec![0x42]], Some((&store, run_id)), None, None)
                .expect("boot_snapshot_and_branch failed");
        let (_root_run_id, _root_node_id_hex) = persisted.expect("root branch point must persist");
        let (_console_output, _ram_hash, mark_branch_step, node_id) = &outcomes[0];
        assert_eq!(*mark_branch_step, Some(1));
        let node_id_hex = node_id.as_ref().expect("first MARK_BRANCH stop must persist").clone();

        let make_spec = || DriverGenerateSpec {
            seed: 99,
            count: 3,
            tape_len_bytes: 2,
            strategy: baud_driver::StrategySpec::default(),
            frame_run_id_prefix: None,
        };

        resume_and_generate(&store, run_id, &node_id_hex, make_spec(), None)
            .expect("first resume_and_generate failed");
        let run = baud_snapshot_store::RunId::new(run_id.to_owned());
        assert!(store.has_driver_state(&run), "generate mode must persist driver state when it persists at all");
        let state_after_first: baud_driver::DriverState =
            serde_json::from_slice(&store.get_driver_state(&run).expect("get_driver_state")).expect("decode state");
        assert_eq!(state_after_first.generation, 3, "generation must advance by spec.count on the first call");
        assert_eq!(state_after_first.reservoir.len(), 3, "every generation's tape should join the reservoir");

        resume_and_generate(&store, run_id, &node_id_hex, make_spec(), None)
            .expect("second resume_and_generate failed");
        let state_after_second: baud_driver::DriverState =
            serde_json::from_slice(&store.get_driver_state(&run).expect("get_driver_state")).expect("decode state");
        assert_eq!(
            state_after_second.generation, 6,
            "a second resume_and_generate call must continue the same Driver's generation counter \
             (3 + 3), not reset it back to 3 with a fresh Driver"
        );
        assert_eq!(
            state_after_second.reservoir.len(),
            6,
            "the reservoir from the first call must carry over, not be discarded"
        );
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
