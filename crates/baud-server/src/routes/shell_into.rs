// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// GET /shell-into/{run_id}/{node_id} — specs/baud-snapshot.md §5's "restore into a live shell":
// reconstruct a persisted `Universe` (the same `reconstruct_universe` `/run/kvm/resume` already
// uses) into a live `Multiverse`, then bridge its console to a WebSocket so a caller gets an
// interactive prompt inside that exact moment instead of a one-shot request/response. This is the
// first route in this crate that keeps a connection open across multiple guest steps — every
// other route (`run_kvm.rs`, `fuzz.rs`, …) is a single bounded `spawn_blocking` call.
//
// The guest loop itself runs on a dedicated blocking thread (`step_exit` is a real `KVM_RUN`
// ioctl — blocking the async executor with it would stall every other request on this process),
// bridged to the WebSocket by two `tokio::sync::mpsc` channels: inbound bytes from the client
// (`input_tx`/`input_rx`) and outbound console growth back to the client (`output_tx`/
// `output_rx`). Shutdown is disconnect-driven, not timer-driven — but disconnect alone is not
// enough to stop immediately: a client that sends a line and then, in the same breath, signals
// "no more input" leaves the guest mid-reaction (still needs real `step_exit` calls to echo it
// back), so `drive_shell_session` keeps stepping for a bounded grace window
// (`POST_DISCONNECT_SETTLE_EXITS`) after the last input was seen before finally giving up — found
// live: an earlier version returned the instant `input_rx` drained to `Disconnected`, even when
// that same drain pass had just enqueued real input the guest never got a single exit to react
// to, so a real interactive client saw its own input echoed back as nothing at all.
//
// That "no more input" signal is a zero-length `Message::Binary` (real console input is never
// empty — this crate's CLI always sends at least a trailing `\r`), never a WebSocket `Close`
// frame, found the hard way: a real close/no-close round trip against a real server process
// showed `tokio-tungstenite` auto-replies to an incoming `Close` the instant it is read, off the
// read task, before the application ever gets a chance to flush pending output — a `send_task`
// that then tries to deliver output the guest produced *after* that point hits "Sending after
// closing is not allowed" and the connection resets instead of closing cleanly. Only the server
// ever sends the real `Close` (`send_task`, once `output_rx` drains dry) — the first and only
// close frame on the connection, so there is no race to lose.
//
// No auth (matches every other route in this crate — `AppState`'s own doc, and this process binds
// `127.0.0.1` only): todo.md's own "and auth" note for this feature remains open, tracked there.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use baud_multiverse::linux::Multiverse;
use baud_snapshot_store::SnapshotStore;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::run_kvm::{reconstruct_universe, WORK_CLOCK_K};
use crate::AppState;

pub async fn shell_into(
    State(state): State<AppState>,
    Path((run_id, node_id)): Path<(String, String)>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let store = state.snapshot_store.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, store, run_id, node_id))
}

async fn handle_socket(socket: WebSocket, store: Arc<SnapshotStore>, run_id: String, node_id: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let guest_task = tokio::task::spawn_blocking(move || {
        run_shell_session(store.as_ref(), &run_id, &node_id, input_rx, output_tx)
    });

    let send_task = tokio::spawn(async move {
        while let Some(bytes) = output_rx.recv().await {
            if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                return;
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    });

    while let Some(msg) = ws_rx.next().await {
        match msg {
            // The "no more input" sentinel (see this module's header doc) — stop reading and let
            // the guest loop drain whatever real input already arrived, but do not send a `Close`
            // ourselves; `send_task` sends the one and only close frame once it has nothing left
            // to deliver.
            Ok(Message::Binary(bytes)) if bytes.is_empty() => break,
            Ok(Message::Binary(bytes)) => {
                let _ = input_tx.send(bytes.into());
            }
            Ok(Message::Text(text)) => {
                let _ = input_tx.send(text.as_bytes().to_vec());
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Err(_) => break,
        }
    }
    drop(input_tx);

    let _ = guest_task.await;
    let _ = send_task.await;
}

/// Reconstruct the persisted universe and hand off to [`drive_shell_session`] — the store-touching
/// half, split out so the interactive loop itself ([`drive_shell_session`]) is unit-testable
/// against a freshly booted `Multiverse` with no `SnapshotStore`/HTTP layer involved at all.
fn run_shell_session(
    store: &SnapshotStore,
    run_id: &str,
    node_id: &str,
    input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    output_tx: mpsc::UnboundedSender<Vec<u8>>,
) -> Result<(), String> {
    let universe = match reconstruct_universe(store, run_id, node_id) {
        Ok(u) => u,
        Err(e) => {
            let _ = output_tx.send(format!("[shell-into: {e}]").into_bytes());
            return Err(e);
        }
    };
    let mut mv = match Multiverse::restore(&universe, Vec::new(), WORK_CLOCK_K, false, None) {
        Ok(mv) => mv,
        Err(e) => {
            let msg = format!("restore error: {e}");
            let _ = output_tx.send(format!("[shell-into: {msg}]").into_bytes());
            return Err(msg);
        }
    };
    drive_shell_session(&mut mv, input_rx, output_tx)
}

/// The guest loop proper — runs on a blocking thread ([`handle_socket`]'s `spawn_blocking`), so
/// every `mv.step_exit()` below is a real, synchronous `KVM_RUN` ioctl, never awaited.
fn drive_shell_session(
    mv: &mut Multiverse,
    mut input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    output_tx: mpsc::UnboundedSender<Vec<u8>>,
) -> Result<(), String> {
    let mut last_len = mv.console_output().len();
    // The captured history itself (`universe.device.console`) so a client sees the same tail
    // `baud-multiverse`'s own `shell_into_universe_resumes` test asserts on, not just growth from
    // this point forward.
    if last_len > 0 {
        let _ = output_tx.send(mv.console_output().to_vec());
    }

    // Once `input_rx` is disconnected, the guest may still be mid-reaction to whatever was
    // enqueued in the very same drain pass that discovered the disconnect (e.g. a client that
    // sends a line then immediately signals "no more input" — found live: a real client doing
    // exactly that got zero output back, because the very first `try_recv` after the disconnect
    // drained the queued line *and* the disconnect together, and an earlier version of this loop
    // returned right then, before `step_exit` ever ran even once). So "disconnected" alone must
    // never end the session immediately — only "disconnected, and nothing new has shown up for a
    // while" does. `settle` counts down real guest-side exits (not wall-clock time) since the
    // last new input arrived; `None` means still connected (no countdown), `Some(0)` means the
    // grace window has fully elapsed with nothing left to react to.
    const POST_DISCONNECT_SETTLE_EXITS: u32 = 200_000;
    let mut settle: Option<u32> = None;

    loop {
        let mut drained_any = false;
        let mut disconnected = false;
        loop {
            match input_rx.try_recv() {
                Ok(bytes) => {
                    mv.enqueue_console_input(&bytes);
                    drained_any = true;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if disconnected {
            settle = match settle {
                _ if drained_any => Some(POST_DISCONNECT_SETTLE_EXITS), // fresh input — full grace window again
                None => Some(POST_DISCONNECT_SETTLE_EXITS),             // first time noticing the disconnect
                Some(0) => return Ok(()), // grace window elapsed, nothing new ever arrived
                Some(n) => Some(n - 1),
            };
        }

        match mv.step_exit() {
            Ok(baud_vcpu::DispatchOutcome::Halted) => {
                flush_new_output(mv, &mut last_len, &output_tx);
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                flush_new_output(mv, &mut last_len, &output_tx);
                let _ = output_tx.send(format!("\r\n[shell-into: determinism hole: {e}]\r\n").into_bytes());
                return Err(e.to_string());
            }
        }
        flush_new_output(mv, &mut last_len, &output_tx);
    }
}

fn flush_new_output(mv: &Multiverse, last_len: &mut usize, output_tx: &mpsc::UnboundedSender<Vec<u8>>) {
    let out = mv.console_output();
    if out.len() > *last_len {
        let _ = output_tx.send(out[*last_len..].to_vec());
        *last_len = out.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn shell_guest_kernel_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../baud-multiverse/tests/fixtures/shell-guest/bzImage")
    }

    /// `drive_shell_session` against a freshly booted (never persisted/restored — the store-facing
    /// half is `reconstruct_universe`'s own well-covered responsibility, `run_kvm.rs`'s tests)
    /// `shell-guest` instance: queued input is echoed and the prompt re-printed exactly as
    /// `baud-multiverse`'s own `shell_into_universe_resumes` proves for the crate-level API this
    /// wraps, and the session ends cleanly once the input channel is dropped and the
    /// post-disconnect settle window elapses with nothing further to react to.
    #[test]
    fn drive_shell_session_echoes_queued_input_and_stops_on_disconnect() {
        let kernel = shell_guest_kernel_path();
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let handle = std::thread::spawn(move || {
            let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, WORK_CLOCK_K, vec![], None)
                .expect("boot failed");
            drive_shell_session(&mut mv, input_rx, output_tx)
        });

        input_tx.send(b"hi\r".to_vec()).expect("send input");

        let target = b"$ hi\n$ ";
        let mut collected = Vec::new();
        while collected.len() < target.len() {
            let chunk = output_rx.blocking_recv().expect("session ended before echoing the queued input");
            collected.extend_from_slice(&chunk);
        }
        assert_eq!(
            collected, target,
            "guest must print its prompt, echo the queued input, then re-prompt"
        );

        // The real "client closed the connection" signal (`handle_socket` drops `input_tx` once
        // its `ws_rx` loop ends) — the session must notice and return, not spin forever.
        drop(input_tx);
        let result = handle.join().expect("session thread panicked");
        assert_eq!(result, Ok(()), "session must end cleanly once the input sender is dropped");
    }

    /// Regression test for a real bug found via manual interactive-mode testing: a client that
    /// sends its input and then, in the very same breath (no gap for the guest to react),
    /// disconnects — exactly what `baud shell-into`'s interactive mode does when stdin closes
    /// right after the last line is typed. An earlier version of `drive_shell_session` returned
    /// the instant `input_rx` drained to `Disconnected`, even when that same drain pass had just
    /// enqueued real input the guest never got a single `step_exit` to react to — the client's own
    /// input vanished into nothing. Sending both lines and dropping the sender with **no** call to
    /// `output_rx.recv()` in between (unlike the test above, which reads before disconnecting)
    /// reproduces that race deterministically instead of relying on real-world scheduling timing.
    #[test]
    fn drive_shell_session_echoes_input_sent_immediately_before_disconnect() {
        let kernel = shell_guest_kernel_path();
        let (input_tx, input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let handle = std::thread::spawn(move || {
            let mut mv = Multiverse::boot(&kernel, "console=ttyS0", 0, WORK_CLOCK_K, vec![], None)
                .expect("boot failed");
            drive_shell_session(&mut mv, input_rx, output_tx)
        });

        // Send both lines and drop the sender back-to-back, before the session thread has had any
        // chance to run — the exact ordering that broke the earlier implementation.
        input_tx.send(b"hi\r".to_vec()).expect("send input");
        input_tx.send(b"bye\r".to_vec()).expect("send input");
        drop(input_tx);

        let target = b"$ hi\n$ bye\n$ ";
        let mut collected = Vec::new();
        while collected.len() < target.len() {
            let chunk = output_rx
                .blocking_recv()
                .expect("session ended before echoing input sent immediately before disconnect");
            collected.extend_from_slice(&chunk);
        }
        assert_eq!(
            collected, target,
            "both lines must be echoed and re-prompted even though the sender was dropped \
             immediately after sending, before the guest ever ran a single step"
        );

        let result = handle.join().expect("session thread panicked");
        assert_eq!(result, Ok(()), "session must still end cleanly once fully settled");
    }
}
