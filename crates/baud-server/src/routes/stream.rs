// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// /stream — M5 frame streaming routes
//
// Routes:
//   GET  /runs/:id/frames         → list frame hashes
//   POST /runs/:id/frames         → append a frame record (from agent)
//   POST /runs/:id/stream/render  → replay with capture, materialise frames (Y4M or QOI-seq)
//   GET  /runs/:id/stream/tail    → live SSE frame stream with terminal completion

use crate::state::unix_now;
use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    response::sse::{Event, Sse},
    Json,
};
use baud_stream::encode_qoi;
use baud_stream::Y4mWriter;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{convert::Infallible, path::PathBuf, time::Duration};

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
) -> Result<Json<Value>, (axum::http::StatusCode, Json<Value>)> {
    let now = unix_now() as i64;
    if body.width == 0 || body.height == 0 {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "frame width and height must be non-zero" })),
        ));
    }
    let pixels = u64::from(body.width).saturating_mul(u64::from(body.height));
    if pixels > 16_777_216 {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "frame geometry exceeds 16 megapixels" })),
        ));
    }
    if !matches!(body.format.as_str(), "indexed8" | "rgb565" | "rgba8888") {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "unsupported frame format" })),
        ));
    }
    let hash_bytes = match hex::decode_hash(&body.hash) {
        Ok(b) => b,
        Err(e) => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({ "error": e })),
            ))
        }
    };

    let result = sqlx::query(
        "INSERT INTO frame_records (run_id, node, step, width, height, format, hash, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
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
        Ok(_) => Ok(Json(
            json!({ "ok": true, "run_id": run_id, "step": body.step }),
        )),
        Err(e) => Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("db error: {e}") })),
        )),
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
) -> Result<Json<Value>, (axum::http::StatusCode, Json<Value>)> {
    let rows = sqlx::query_as::<_, (i64, i64, i64, i64, String, Vec<u8>)>(
        "SELECT node, step, width, height, format, hash
         FROM frame_records
         WHERE run_id = ?
           AND (? IS NULL OR node = ?)
           AND (? IS NULL OR step >= ?)
           AND (? IS NULL OR step <= ?)
         ORDER BY step ASC",
    )
    .bind(&run_id)
    .bind(q.node)
    .bind(q.node)
    .bind(q.from_step)
    .bind(q.from_step)
    .bind(q.to_step)
    .bind(q.to_step)
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let frames: Vec<Value> = rows
                .into_iter()
                .map(|(node, step, w, h, fmt, hash)| {
                    json!({
                        "node": node,
                        "step": step,
                        "width": w,
                        "height": h,
                        "format": fmt,
                        "hash": hex_encode(&hash),
                    })
                })
                .collect();
            Ok(Json(json!({ "run_id": run_id, "frames": frames })))
        }
        Err(e) => Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("db error: {e}") })),
        )),
    }
}

// ---------------------------------------------------------------------------
// POST /runs/:id/stream/render — materialise frames from stored frame data
//
// When `kvm_run_meta` has a row for this run (a real `/run/kvm { run_id: ... }` boot,
// todo.md §14's eighteenth-brick follow-up), this re-boots that exact kernel/cmdline/tape under
// baud-multiverse and writes the real pixel bytes the guest produced. Hash-only records without
// replay metadata are rejected because a hash cannot be decoded back into guest pixels.
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
    let kvm_meta = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            bool,
            Option<String>,
            Option<i64>,
            Option<i64>,
        ),
    >(
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
        Ok(None) => Err(json!({
            "error": "frame pixels are unavailable: this run has no replayable KVM image and stored hashes cannot be rendered"
        })),
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
                writer
                    .write_frame(rgba)
                    .map_err(|e| format!("Y4mWriter frame: {e}"))?;
            }
            writer
                .finish()
                .map_err(|e| format!("Y4mWriter finish: {e}"))?;
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
        None => {
            return Err(
                json!({ "error": "stored tape_hex is not valid hex (corrupt kvm_run_meta row)" }),
            )
        }
    };
    let initramfs = match &initramfs_path {
        Some(path) => match crate::routes::run_kvm::read_initramfs(path) {
            Ok(bytes) => Some(bytes),
            Err(e) => return Err(json!({ "error": e })),
        },
        None => None,
    };
    // Mapped, never read onto the heap — the same fix as `/run/kvm`'s own path; see
    // `run_kvm::open_virtio_blk_image`'s doc. A replay boots the identical image the original run
    // did, so it carried the identical double-copy cost until now.
    let virtio_blk_image = match &virtio_blk {
        Some((path, _, _)) => match crate::routes::run_kvm::open_virtio_blk_image(
            path,
            crate::routes::run_kvm::virtio_blk_image_size_limit(),
        ) {
            Ok(base) => Some(base),
            Err(e) => return Err(json!({ "error": e })),
        },
        None => None,
    };
    let virtio_blk_meta = virtio_blk.map(|(_, v, m)| (v, m));
    let kernel_path_buf = PathBuf::from(&kernel_path);
    // Same client-disconnect cancellation as `/run/kvm`: a replay is a full KVM boot.
    let cancel = crate::routes::run_kvm::CancelGuard::new();
    let cancel_flag = cancel.flag();
    let records = tokio::task::spawn_blocking(move || {
        let virtio_blk = match (virtio_blk_image, virtio_blk_meta) {
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
            Some(cancel_flag),
        )
    })
    .await
    .map_err(|error| json!({ "error": format!("replay task failed: {error}") }))?;
    drop(cancel); // held across the `.await` above on purpose — that is the whole mechanism

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
async fn render_frames_from_real_restore(
    params: RealRestoreParams,
) -> Result<Vec<(u32, u32, Vec<u8>)>, Value> {
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
        None => {
            return Err(
                json!({ "error": "stored tape_hex is not valid hex (corrupt kvm_run_meta row)" }),
            )
        }
    };
    // Keep the ownership guard across the blocking restore/replay just like the reboot path.
    let cancel = crate::routes::run_kvm::CancelGuard::new();
    let cancel_flag = cancel.flag();
    let records = tokio::task::spawn_blocking(move || -> Result<Vec<baud_proto::Msg>, String> {
        let universe =
            crate::routes::run_kvm::reconstruct_universe(&store, &store_run_id, &snapshot_node_id)?;
        let mut branch = baud_multiverse::linux::Multiverse::branch(
            &universe,
            tape_suffix,
            crate::routes::run_kvm::WORK_CLOCK_K,
            None,
        )
        .map_err(|e| format!("restore branch error: {e}"))?;
        branch.set_cancel_flag(cancel_flag);
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
    .map_err(|error| json!({ "error": format!("restore task failed: {error}") }))?;
    drop(cancel);

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
async fn render_frames_from_real_restore(
    _params: RealRestoreParams,
) -> Result<Vec<(u32, u32, Vec<u8>)>, Value> {
    Err(json!({ "error": "real KVM restore-replay is only available on target_os = \"linux\"" }))
}

// ---------------------------------------------------------------------------
// GET /runs/:id/stream/tail — live frames
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
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let hashes_only = q.hashes_only.unwrap_or(false);
    let node = q.node;
    let stream = futures_util::stream::unfold(
        (state, run_id, node, 0_i64, false),
        move |(state, run_id, node, last_id, terminal)| async move {
            if terminal {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            let rows = match sqlx::query_as::<_, (i64, i64, i64, i64, String, Vec<u8>, i64)>(
                "SELECT node, step, width, height, format, hash, id
             FROM frame_records
             WHERE run_id = ? AND (? IS NULL OR node = ?) AND id > ?
             ORDER BY id ASC",
            )
            .bind(&run_id)
            .bind(node)
            .bind(node)
            .bind(last_id)
            .fetch_all(&state.db)
            .await
            {
                Ok(rows) => rows,
                Err(error) => {
                    let event = Event::default()
                        .event("error")
                        .json_data(json!({"error": format!("frame stream query failed: {error}")}))
                        .unwrap_or_else(|_| {
                            Event::default().data("{\"error\":\"frame stream query failed\"}")
                        });
                    return Some((Ok(event), (state, run_id, node, last_id, true)));
                }
            };
            let next_id = rows
                .iter()
                .map(|(_, _, _, _, _, _, id)| *id)
                .max()
                .unwrap_or(last_id);
            if rows.is_empty() {
                // A tail must not leave clients polling forever after a run has reached a
                // terminal state. Heartbeats keep a live run observable, while `done` closes
                // the SSE stream once the durable run status says no more frames can arrive.
                let status =
                    sqlx::query_scalar::<_, String>("SELECT status FROM runs WHERE id = ?")
                        .bind(&run_id)
                        .fetch_optional(&state.db)
                        .await;
                match status {
                    Ok(Some(status)) if is_terminal_run_status(&status) => {
                        let event = Event::default()
                            .event("done")
                            .json_data(json!({
                                "run_id": run_id,
                                "status": status,
                            }))
                            .unwrap_or_else(|_| Event::default().data("{}"));
                        Some((Ok(event), (state, run_id, node, last_id, true)))
                    }
                    Ok(None) => {
                        let event = Event::default()
                            .event("error")
                            .json_data(json!({
                                "error": format!("run {} not found", run_id),
                            }))
                            .unwrap_or_else(|_| Event::default().data("{}"));
                        Some((Ok(event), (state, run_id, node, last_id, true)))
                    }
                    Ok(Some(_)) | Err(_) => {
                        let event = Event::default().event("heartbeat").data("{}");
                        Some((Ok(event), (state, run_id, node, last_id, false)))
                    }
                }
            } else {
                let data: Vec<Value> = rows.into_iter().map(|(n, step, w, h, fmt, hash, _id)| {
                if hashes_only {
                    json!({ "run_id": run_id, "node": n, "step": step, "hash": hex_encode(&hash) })
                } else {
                    json!({ "run_id": run_id, "node": n, "step": step, "width": w, "height": h, "format": fmt, "hash": hex_encode(&hash) })
                }
            }).collect();
                Some((
                    Ok(Event::default()
                        .event("frame")
                        .json_data(data)
                        .unwrap_or_else(|_| Event::default().data("{}"))),
                    (state, run_id, node, next_id, false),
                ))
            }
        },
    );
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_terminal_run_status(status: &str) -> bool {
    matches!(
        status,
        "done" | "failed" | "aborted" | "divergent" | "error"
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Strict hex decode for a stored `kvm_run_meta.tape_hex` value. Unlike the frame-hash decoder
/// above, this accepts only the exact hexadecimal tape representation persisted by KVM runs,
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
    use base64::Engine;

    pub fn decode_hash(s: &str) -> Result<Vec<u8>, String> {
        let bytes = if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "hash is not valid hexadecimal".to_string())?
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|_| "hash must be a 32-byte hexadecimal or base64 value".to_string())?
        };
        if bytes.len() != 32 {
            return Err(format!(
                "frame hash must contain exactly 32 bytes, got {}",
                bytes.len()
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::{hex::decode_hash, is_terminal_run_status};
    use base64::Engine;

    #[test]
    fn terminal_run_statuses_close_tail_and_nonterminal_statuses_keep_it_open() {
        for status in ["done", "failed", "aborted", "divergent", "error"] {
            assert!(is_terminal_run_status(status), "{status} must close a tail");
        }
        for status in ["pending", "provisioning", "running"] {
            assert!(
                !is_terminal_run_status(status),
                "{status} must keep a tail live"
            );
        }
    }

    #[test]
    fn frame_hash_decoder_requires_32_bytes() {
        assert!(decode_hash("00").is_err());
        assert_eq!(decode_hash(&"ab".repeat(32)).unwrap().len(), 32);
        let encoded = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert_eq!(decode_hash(&encoded).unwrap().len(), 32);
    }
}
