// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /stream — M5 frame streaming routes
//
// Routes:
//   GET  /runs/:id/frames         → list frame hashes
//   POST /runs/:id/frames         → append a frame record (from agent)
//   POST /runs/:id/stream/render  → replay with capture, materialise frames (Y4M or QOI-seq)
//   GET  /runs/:id/stream/tail    → SSE-like frame stream (returns list for now)

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use crate::AppState;
use crate::state::unix_now;
use baud_stream::Y4mWriter;
use baud_stream::encode_qoi;

// ---------------------------------------------------------------------------
// POST /runs/:id/frames — append a frame record
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AppendFrameBody {
    pub node: u16,
    pub step: u64,
    pub width: u32,
    pub height: u32,
    pub format: String,
    /// Base64-encoded blake3 hash (32 bytes)
    pub hash: String,
}

pub async fn append_frame(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<AppendFrameBody>,
) -> Json<Value> {
    let now = unix_now() as i64;
    let hash_bytes = match hex::decode_or_b64(&body.hash) {
        Some(b) => b,
        None => return Json(json!({ "error": "invalid hash encoding" })),
    };

    let result = sqlx::query(
        "INSERT INTO frame_records (run_id, node, step, width, height, format, hash, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&run_id)
    .bind(body.node as i64)
    .bind(body.step as i64)
    .bind(body.width as i64)
    .bind(body.height as i64)
    .bind(&body.format)
    .bind(&hash_bytes)
    .bind(now)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(json!({ "ok": true, "run_id": run_id, "step": body.step })),
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// GET /runs/:id/frames — list frame hashes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FramesQuery {
    pub node: Option<i64>,
    pub from_step: Option<i64>,
    pub to_step: Option<i64>,
}

pub async fn list_frames(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<FramesQuery>,
) -> Json<Value> {
    let rows = sqlx::query_as::<_, (i64, i64, i64, i64, String, Vec<u8>)>(
        "SELECT node, step, width, height, format, hash
         FROM frame_records
         WHERE run_id = ?
           AND (? IS NULL OR node = ?)
           AND (? IS NULL OR step >= ?)
           AND (? IS NULL OR step <= ?)
         ORDER BY step ASC"
    )
    .bind(&run_id)
    .bind(q.node).bind(q.node)
    .bind(q.from_step).bind(q.from_step)
    .bind(q.to_step).bind(q.to_step)
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let frames: Vec<Value> = rows.into_iter().map(|(node, step, w, h, fmt, hash)| {
                json!({
                    "node": node,
                    "step": step,
                    "width": w,
                    "height": h,
                    "format": fmt,
                    "hash": hex_encode(&hash),
                })
            }).collect();
            Json(json!({ "run_id": run_id, "frames": frames }))
        }
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// POST /runs/:id/stream/render — materialise frames from stored frame data
//
// When `kvm_run_meta` has a row for this run (a real `/run/kvm { run_id: ... }` boot,
// todo.md §14's eighteenth-brick follow-up), this re-boots that exact kernel/cmdline/tape under
// baud-multiverse and writes the *real* pixel bytes the guest produced. Runs with no such row —
// every pre-pivot manually-seeded run (`POST /runs/:id/frames`, hash-only, no kernel/tape to
// replay) — keep the prior synthetic-gradient-from-hash fallback so existing callers of that
// route (drive/m/m5.sh, m8.sh, full-demo.sh) are unaffected.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RenderBody {
    pub from_step: Option<u64>,
    pub to_step: Option<u64>,
    pub format: Option<String>, // "y4m" or "qoi-seq"
    pub out: Option<String>,
}

pub async fn render(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<RenderBody>,
) -> Json<Value> {
    let from_step = body.from_step.unwrap_or(0);
    let to_step = body.to_step;
    let fmt = body.format.as_deref().unwrap_or("y4m").to_string();
    let out_path = body.out.as_deref().unwrap_or("output.y4m").to_string();

    #[allow(clippy::type_complexity)]
    let kvm_meta = sqlx::query_as::<_, (String, String, String, Option<String>, Option<i64>, Option<i64>, Option<i64>, Option<String>, Option<String>, Option<i64>, Option<i64>, Option<i64>, bool, Option<String>, Option<i64>, Option<i64>)>(
        "SELECT kernel_path, cmdline, tape_hex, initramfs_path, periodic_timer_period_rcb, \
         periodic_timer_vector, periodic_timer_max_ticks, store_run_id, snapshot_node_id, \
         virtio_rng_seed, virtio_rng_vector, virtio_rng_max_exits, acpi, \
         virtio_blk_image_path, virtio_blk_vector, virtio_blk_max_exits \
         FROM kvm_run_meta WHERE run_id = ?",
    )
    .bind(&run_id)
    .fetch_optional(&state.db)
    .await;

    let frames: Result<Vec<(u32, u32, Vec<u8>)>, Value> = match kvm_meta {
        Ok(Some((
            kernel_path,
            cmdline,
            tape_hex,
            initramfs_path,
            period_rcb,
            vector,
            max_ticks,
            store_run_id,
            snapshot_node_id,
            rng_seed,
            rng_vector,
            rng_max_exits,
            acpi,
            blk_image_path,
            blk_vector,
            blk_max_exits,
        ))) => {
            let periodic_timer = match (period_rcb, vector, max_ticks) {
                (Some(p), Some(v), Some(m)) => Some((p as u64, v as u8, m as u32)),
                _ => None,
            };
            let virtio_rng = match (rng_seed, rng_vector, rng_max_exits) {
                (Some(s), Some(v), Some(m)) => Some((s as u64, v as u8, m as u32)),
                _ => None,
            };
            let virtio_blk = match (blk_image_path, blk_vector, blk_max_exits) {
                (Some(p), Some(v), Some(m)) => Some((p, v as u8, m as u32)),
                _ => None,
            };
            // A resume-originated run (todo.md §14's "`/run/kvm/resume`'s lineage gap") has no
            // kernel to reboot — `store_run_id`/`snapshot_node_id` name the `Universe` to restore
            // from `SnapshotStore` instead, with `tape_hex` as the suffix to feed it. Every
            // reboot-based row (`run()`/`branch()`) leaves both `NULL`, so this is mutually
            // exclusive with the `kernel_path`/`cmdline` reboot path below, never both.
            match (store_run_id, snapshot_node_id) {
                (Some(store_run_id), Some(snapshot_node_id)) => {
                    render_frames_from_real_restore(RealRestoreParams {
                        store: state.snapshot_store.clone(),
                        store_run_id,
                        snapshot_node_id,
                        tape_suffix_hex: tape_hex,
                        periodic_timer,
                        virtio_rng,
                        from_step,
                        to_step,
                    })
                    .await
                }
                _ => {
                    render_frames_from_real_replay(RealReplayParams {
                        kernel_path,
                        cmdline,
                        tape_hex,
                        initramfs_path,
                        periodic_timer,
                        virtio_rng,
                        virtio_blk,
                        acpi,
                        from_step,
                        to_step,
                    })
                    .await
                }
            }
        }
        Ok(None) => render_frames_from_stored_hashes(&state, &run_id, from_step, to_step).await,
        Err(e) => Err(json!({ "error": format!("db error: {e}") })),
    };

    let frames = match frames {
        Ok(frames) => frames,
        Err(e) => return Json(e),
    };
    if frames.is_empty() {
        return Json(json!({ "error": "no frames found for this run/range" }));
    }
    let (w, h, _) = &frames[0];
    let (w, h) = (*w, *h);

    let render_result: Result<(Vec<u8>, usize), String> = (|| {
        let mut output: Vec<u8> = Vec::new();

        if fmt == "y4m" || fmt == "yuv4mpeg2" {
            let mut writer = Y4mWriter::new(&mut output, w, h, 30, 1)
                .map_err(|e| format!("Y4mWriter init failed: {e}"))?;
            for (_, _, rgba) in &frames {
                writer.write_frame(rgba).map_err(|e| format!("Y4mWriter frame: {e}"))?;
            }
            writer.finish().map_err(|e| format!("Y4mWriter finish: {e}"))?;
        } else {
            // QOI sequence: each frame is a standalone QOI image concatenated
            for (fw, fh, rgba) in &frames {
                let qoi = encode_qoi(rgba, *fw, *fh).map_err(|e| format!("QOI encode: {e}"))?;
                output.extend_from_slice(&qoi);
            }
        }

        let n = frames.len();
        Ok((output, n))
    })();

    match render_result {
        Ok((bytes, n)) => {
            let write_result = std::fs::write(&out_path, &bytes);
            match write_result {
                Ok(()) => Json(json!({
                    "ok": true,
                    "run_id": run_id,
                    "format": fmt,
                    "out": out_path,
                    "width": w,
                    "height": h,
                    "frame_count": n,
                    "bytes_written": bytes.len(),
                    "from_step": from_step,
                    "to_step": to_step,
                })),
                Err(e) => Json(json!({
                    "ok": false,
                    "error": format!("could not write {out_path}: {e}"),
                    "frame_count": n,
                    "bytes_generated": bytes.len(),
                })),
            }
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

/// Bundles `render_frames_from_real_replay`'s params — kept as a struct rather than 8 positional
/// args, same convention as `run_kvm::KvmBootParams`, to stay under clippy's `too_many_arguments`.
#[cfg(target_os = "linux")]
struct RealReplayParams {
    kernel_path: String,
    cmdline: String,
    tape_hex: String,
    initramfs_path: Option<String>,
    periodic_timer: Option<(u64, u8, u32)>,
    virtio_rng: Option<(u64, u8, u32)>,
    /// `(image_path, vector, max_exits)` — see `run_kvm::RunKvmBody::virtio_blk`'s doc.
    virtio_blk: Option<(String, u8, u32)>,
    acpi: bool,
    from_step: u64,
    to_step: Option<u64>,
}

/// Real replay: re-boot the exact kernel/cmdline/tape a `/run/kvm { run_id: ... }` call recorded
/// in `kvm_run_meta`, drain the real `Msg::Frame` records it produces (raw pixel bytes included —
/// `FrameRecord::bytes` is always `Some` for a live boot, `baud_multiverse::linux::Multiverse::
/// drain_tape_records`'s doc), and convert each to RGBA with `baud_stream::to_rgba` — the same
/// conversion `baud-stream`'s own fingerprinting/encoding path uses, so a real guest's `Indexed8`/
/// `Rgb565` frames render exactly as `specs/baud-stream.md` describes instead of a synthetic
/// hash-seeded gradient.
#[cfg(target_os = "linux")]
async fn render_frames_from_real_replay(
    params: RealReplayParams,
) -> Result<Vec<(u32, u32, Vec<u8>)>, Value> {
    let RealReplayParams {
        kernel_path,
        cmdline,
        tape_hex,
        initramfs_path,
        periodic_timer,
        virtio_rng,
        virtio_blk,
        acpi,
        from_step,
        to_step,
    } = params;
    let tape = match hex_decode(&tape_hex) {
        Some(t) => t,
        None => return Err(json!({ "error": "stored tape_hex is not valid hex (corrupt kvm_run_meta row)" })),
    };
    let initramfs = match &initramfs_path {
        Some(path) => match crate::routes::run_kvm::read_initramfs(path) {
            Ok(bytes) => Some(bytes),
            Err(e) => return Err(json!({ "error": e })),
        },
        None => None,
    };
    let virtio_blk_image = match &virtio_blk {
        Some((path, _, _)) => match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) => return Err(json!({ "error": format!("failed to read virtio_blk image_path '{path}': {e}") })),
        },
        None => None,
    };
    let virtio_blk_meta = virtio_blk.map(|(_, v, m)| (v, m));
    let kernel_path_buf = PathBuf::from(&kernel_path);
    let records = tokio::task::spawn_blocking(move || {
        let virtio_blk = match (virtio_blk_image.as_deref(), virtio_blk_meta) {
            (Some(image), Some((vector, max_exits))) => Some((image, vector, max_exits)),
            _ => None,
        };
        crate::routes::run_kvm::boot_and_drain_frames(
            &kernel_path_buf,
            &cmdline,
            tape,
            initramfs.as_deref(),
            periodic_timer,
            virtio_rng,
            virtio_blk,
            acpi,
        )
    })
    .await
    .expect("stream/render replay task panicked");

    let records = match records {
        Ok(records) => records,
        Err(e) => return Err(json!({ "error": format!("replay error: {e}") })),
    };

    Ok(records
        .into_iter()
        .filter(|r| r.step >= from_step && to_step.is_none_or(|to| r.step <= to))
        .filter_map(|r| {
            let bytes = r.bytes?;
            let rgba = baud_stream::to_rgba(&bytes, &r.format);
            Some((r.width, r.height, rgba))
        })
        .collect())
}

/// `/run/kvm` (and thus `kvm_run_meta`) only exists on `target_os = "linux"` (`routes/mod.rs`'s
/// own `#[cfg(target_os = "linux")] pub mod run_kvm;`) — this workspace only ever builds/runs on
/// real Linux+KVM hosts (`CLAUDE.md`), but `stream.rs` itself is not Linux-gated, so this stub
/// keeps a non-Linux `cargo check` compiling instead of failing on the Linux-only call below.
#[cfg(not(target_os = "linux"))]
struct RealReplayParams {
    kernel_path: String,
    cmdline: String,
    tape_hex: String,
    initramfs_path: Option<String>,
    periodic_timer: Option<(u64, u8, u32)>,
    virtio_rng: Option<(u64, u8, u32)>,
    virtio_blk: Option<(String, u8, u32)>,
    acpi: bool,
    from_step: u64,
    to_step: Option<u64>,
}

#[cfg(not(target_os = "linux"))]
async fn render_frames_from_real_replay(
    _params: RealReplayParams,
) -> Result<Vec<(u32, u32, Vec<u8>)>, Value> {
    Err(json!({ "error": "real KVM replay is only available on target_os = \"linux\"" }))
}

/// Restore-and-replay: `POST /run/kvm/resume`'s counterpart to `render_frames_from_real_replay`,
/// for a run that has no kernel to reboot (`kvm_run_meta.store_run_id`/`snapshot_node_id` set
/// instead of a real `kernel_path`/`cmdline`, see `render()`'s own doc). Reconstructs the
/// `Universe` at `(store_run_id, snapshot_node_id)` from `SnapshotStore` exactly as
/// `routes::run_kvm::reconstruct_universe` does for a live `/run/kvm/resume` call, forks it with
/// `tape_hex` as a tape *suffix* via `Multiverse::branch` (not a whole-boot tape — this is the same
/// `WORK_CLOCK_K`/`Multiverse::branch` primitive `resume_and_branch` uses, so this reproduces
/// exactly what that live call did), runs it to its first halt/`MARK_BRANCH`, and drains the real
/// `Msg::Frame` records it produces — the restore-based analogue of the reboot-based path, closing
/// todo.md §14's "`/run/kvm/resume`'s lineage gap" (no per-node full-tape-from-root reconstruction
/// needed: only this one node's own tape suffix, which `RunKvmResumeBody::frame_run_ids`/
/// `DriverGenerateSpec::frame_run_id_prefix` now persist). `virtio_rng`, when set, re-enables and
/// re-seeds the device fresh on the forked `Multiverse::branch` (device state is not itself part of
/// the snapshot/restore/branch contract, see `Multiverse::run_until_branch_or_halt_with_virtio_rng`'s
/// doc) and dispatches to the matching `..._with_virtio_rng` combinator — this closes the last
/// still-open piece of todo.md §14 next-actions item 1's virtio-rng gap: `render()`'s reboot path
/// (`render_frames_from_real_replay`) already threaded `virtio_rng` through; this restore path did not.
#[cfg(target_os = "linux")]
struct RealRestoreParams {
    store: std::sync::Arc<baud_snapshot_store::SnapshotStore>,
    store_run_id: String,
    snapshot_node_id: String,
    tape_suffix_hex: String,
    periodic_timer: Option<(u64, u8, u32)>,
    virtio_rng: Option<(u64, u8, u32)>,
    from_step: u64,
    to_step: Option<u64>,
}

#[cfg(target_os = "linux")]
async fn render_frames_from_real_restore(params: RealRestoreParams) -> Result<Vec<(u32, u32, Vec<u8>)>, Value> {
    let RealRestoreParams {
        store,
        store_run_id,
        snapshot_node_id,
        tape_suffix_hex,
        periodic_timer,
        virtio_rng,
        from_step,
        to_step,
    } = params;
    let tape_suffix = match hex_decode(&tape_suffix_hex) {
        Some(t) => t,
        None => return Err(json!({ "error": "stored tape_hex is not valid hex (corrupt kvm_run_meta row)" })),
    };
    let records = tokio::task::spawn_blocking(move || -> Result<Vec<baud_proto::Msg>, String> {
        let universe = crate::routes::run_kvm::reconstruct_universe(&store, &store_run_id, &snapshot_node_id)?;
        let mut branch = baud_multiverse::linux::Multiverse::branch(
            &universe,
            tape_suffix,
            crate::routes::run_kvm::WORK_CLOCK_K,
            None,
        )
        .map_err(|e| format!("restore branch error: {e}"))?;
        if let Some((seed, _, _)) = virtio_rng {
            branch.enable_virtio_rng();
            branch.seed_virtio_rng_entropy(seed);
        }
        let mut records = match (periodic_timer, virtio_rng) {
            (Some((period_rcb, timer_vector, max_ticks)), Some((_, rng_vector, _))) => {
                let (_ticks, _outcome, records) = branch
                    .run_until_branch_or_halt_with_periodic_timer_and_virtio_rng(
                        period_rcb,
                        timer_vector,
                        rng_vector,
                        max_ticks,
                    )
                    .map_err(|e| format!("determinism hole: {e}"))?;
                records
            }
            (Some((period_rcb, vector, max_ticks)), None) => {
                let (_ticks, _outcome, records) = branch
                    .run_until_branch_or_halt_with_periodic_timer(period_rcb, vector, max_ticks)
                    .map_err(|e| format!("determinism hole: {e}"))?;
                records
            }
            (None, Some((_, rng_vector, max_exits))) => {
                let (_outcome, records) = branch
                    .run_until_branch_or_halt_with_virtio_rng(rng_vector, max_exits)
                    .map_err(|e| format!("determinism hole: {e}"))?;
                records
            }
            (None, None) => {
                let (_outcome, records) = branch
                    .run_until_branch_or_halt(crate::routes::run_kvm::BRANCH_MAX_EXITS)
                    .map_err(|e| format!("determinism hole: {e}"))?;
                records
            }
        };
        records.extend(branch.drain_tape_records());
        Ok(records)
    })
    .await
    .expect("stream/render restore task panicked");

    let records = match records {
        Ok(records) => records,
        Err(e) => return Err(json!({ "error": format!("restore-replay error: {e}") })),
    };

    Ok(records
        .into_iter()
        .filter_map(|m| match m {
            baud_proto::Msg::Frame(frame) => Some(frame),
            _ => None,
        })
        .filter(|r| r.step >= from_step && to_step.is_none_or(|to| r.step <= to))
        .filter_map(|r| {
            let bytes = r.bytes?;
            let rgba = baud_stream::to_rgba(&bytes, &r.format);
            Some((r.width, r.height, rgba))
        })
        .collect())
}

/// Non-Linux stub, mirroring `render_frames_from_real_replay`'s own — see its doc for why.
#[cfg(not(target_os = "linux"))]
struct RealRestoreParams {
    store: std::sync::Arc<baud_snapshot_store::SnapshotStore>,
    store_run_id: String,
    snapshot_node_id: String,
    tape_suffix_hex: String,
    periodic_timer: Option<(u64, u8, u32)>,
    virtio_rng: Option<(u64, u8, u32)>,
    from_step: u64,
    to_step: Option<u64>,
}

#[cfg(not(target_os = "linux"))]
async fn render_frames_from_real_restore(_params: RealRestoreParams) -> Result<Vec<(u32, u32, Vec<u8>)>, Value> {
    Err(json!({ "error": "real KVM restore-replay is only available on target_os = \"linux\"" }))
}

/// Pre-pivot fallback: the stored frame records contain only content hashes (the agent omits raw
/// pixels to save bandwidth, `frame_records`'s "bytes are NOT stored here" convention) with no
/// kernel/tape on record to replay — derive a deterministic synthetic frame from the stored hash
/// instead, exactly as this route always has (VR2-M19: render writes real, reproducible bytes,
/// just not the guest's *actual* pixels, since nothing recorded what those were).
async fn render_frames_from_stored_hashes(
    state: &AppState,
    run_id: &str,
    from_step: u64,
    to_step: Option<u64>,
) -> Result<Vec<(u32, u32, Vec<u8>)>, Value> {
    let from_step = from_step as i64;
    let to_step_val = to_step.map(|v| v as i64);
    let rows = sqlx::query_as::<_, (i64, i64, i64, i64, String, Vec<u8>)>(
        "SELECT node, step, width, height, format, hash
         FROM frame_records
         WHERE run_id = ? AND step >= ? AND (? IS NULL OR step <= ?)
         ORDER BY step ASC"
    )
    .bind(run_id)
    .bind(from_step)
    .bind(to_step_val).bind(to_step_val)
    .fetch_all(&state.db)
    .await
    .map_err(|e| json!({ "error": format!("db error: {e}") }))?;

    Ok(rows
        .into_iter()
        .map(|(_, _, w, h, _, hash)| {
            let w = w as u32;
            let h = h as u32;
            (w, h, synthetic_frame_rgba(&hash, w, h))
        })
        .collect())
}

/// Generate a deterministic synthetic RGBA frame from a frame hash.
///
/// The hash acts as a seed so the same stored hash always produces the same
/// pixels. The gradient pattern makes frames visually distinguishable.
fn synthetic_frame_rgba(hash: &[u8], width: u32, height: u32) -> Vec<u8> {
    // Use first 3 bytes of hash as colour offset
    let r_off = hash.first().copied().unwrap_or(0);
    let g_off = hash.get(1).copied().unwrap_or(0);
    let b_off = hash.get(2).copied().unwrap_or(0);

    let w = width as usize;
    let h = height as usize;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let r = r_off.wrapping_add((x * 255 / w.max(1)) as u8);
            let g = g_off.wrapping_add((y * 255 / h.max(1)) as u8);
            let b = b_off.wrapping_add(((x + y) * 127 / (w + h).max(1)) as u8);
            rgba.push(r);
            rgba.push(g);
            rgba.push(b);
            rgba.push(255); // alpha
        }
    }
    rgba
}

// ---------------------------------------------------------------------------
// GET /runs/:id/stream/tail — live frames (returns stored list for now)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TailQuery {
    pub node: Option<i64>,
    pub hashes_only: Option<bool>,
}

pub async fn tail(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(q): Query<TailQuery>,
) -> Json<Value> {
    let hashes_only = q.hashes_only.unwrap_or(false);

    let rows = sqlx::query_as::<_, (i64, i64, i64, i64, String, Vec<u8>)>(
        "SELECT node, step, width, height, format, hash
         FROM frame_records
         WHERE run_id = ? AND (? IS NULL OR node = ?)
         ORDER BY step ASC"
    )
    .bind(&run_id)
    .bind(q.node).bind(q.node)
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let frames: Vec<Value> = rows.into_iter().map(|(node, step, w, h, fmt, hash)| {
                if hashes_only {
                    json!({ "node": node, "step": step, "hash": hex_encode(&hash) })
                } else {
                    json!({
                        "node": node,
                        "step": step,
                        "width": w,
                        "height": h,
                        "format": fmt,
                        "hash": hex_encode(&hash),
                    })
                }
            }).collect();
            Json(json!({ "run_id": run_id, "frames": frames }))
        }
        Err(e) => Json(json!({ "error": format!("db error: {e}") })),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Strict hex decode for a stored `kvm_run_meta.tape_hex` value — unlike `hex::decode_or_b64`
/// below (which exists to accept loose test-seeded hash strings on `POST /runs/:id/frames`),
/// this must never silently treat malformed input as raw bytes: it feeds directly into
/// `Multiverse::boot`'s tape.
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

// Simple hex / base64 decode helper
mod hex {
    pub fn decode_or_b64(s: &str) -> Option<Vec<u8>> {
        // Try hex first
        if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            let bytes: Option<Vec<u8>> = (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
                .collect();
            if let Some(b) = bytes {
                if b.len() == 32 {
                    return Some(b);
                }
            }
        }
        // Accept any non-empty byte string as a fallback (for test data)
        if !s.is_empty() {
            return Some(s.as_bytes().to_vec());
        }
        None
    }
}
