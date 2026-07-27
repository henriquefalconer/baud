// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud shell-into — specs/baud-snapshot.md §5's "restore into a live shell": connect to
// `GET /shell-into/{run_id}/{node_id}` (`crates/baud-server/src/routes/shell_into.rs`) and bridge
// it to this process's own stdin/stdout, or, with `--input-hex`, run one scripted round trip and
// exit — the mode `drive/h5b.sh` uses, since a shell script has no real TTY to drive interactively.

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

use crate::{client::Client, fmt};

#[derive(Parser)]
pub struct ShellIntoArgs {
    /// Run ID the universe was persisted under (`persisted.run_id` from `/run/kvm/branch`)
    pub run_id: String,
    /// Node ID identifying the universe to resume into (`persisted.node_id`)
    pub node_id: String,
    /// Non-interactive mode: send these hex-encoded bytes once, collect every byte the guest
    /// writes back until `--idle-timeout-ms` passes with nothing new, then print
    /// `{"ok":true,"output_hex":...}` and exit — for scripts, since they have no real stdin/stdout
    /// TTY to pipe interactively. Omit for a real interactive session (reads stdin line by line,
    /// prints guest output to stdout as it arrives, until stdin closes).
    #[arg(long)]
    pub input_hex: Option<String>,
    /// How long to wait for further guest output after the last byte received before treating the
    /// session as quiescent and closing (`--input-hex` mode only).
    #[arg(long, default_value_t = 2000)]
    pub idle_timeout_ms: u64,
    /// How long to wait for the *first* byte of guest output before giving up (`--input-hex` mode
    /// only). Kept separate from `--idle-timeout-ms`: that one measures "the guest stopped
    /// talking", which is fast once output has started, while this one measures "restore +
    /// stepping the guest to its first output hasn't happened yet", which can take much longer
    /// under concurrent host load (e.g. several other guests booting at once) without the session
    /// itself being unhealthy.
    #[arg(long, default_value_t = 10000)]
    pub first_byte_timeout_ms: u64,
}

pub async fn run(args: ShellIntoArgs, c: &Client, json: bool) -> Result<()> {
    let url = c.ws_url(&format!("/shell-into/{}/{}", args.run_id, args.node_id));
    let (ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connect {url}: could not reach baud-server"))?;
    let (mut tx, mut rx) = ws.split();

    match args.input_hex {
        Some(hex) => {
            let bytes = hex_decode(&hex).context("--input-hex must be a valid hex string")?;
            tx.send(Message::Binary(bytes.into())).await.context("send input")?;

            let mut output = Vec::new();
            loop {
                // Before any output has arrived, the guest may still be mid-restore under
                // concurrent host load, so wait up to `first_byte_timeout_ms`; once it has
                // started talking, a gap means it is genuinely quiescent, so switch to the
                // shorter `idle_timeout_ms`.
                let timeout_ms = if output.is_empty() {
                    args.first_byte_timeout_ms
                } else {
                    args.idle_timeout_ms
                };
                let next = tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    rx.next(),
                )
                .await;
                match next {
                    Ok(Some(Ok(Message::Binary(bytes)))) => output.extend_from_slice(&bytes),
                    Ok(Some(Ok(Message::Text(text)))) => output.extend_from_slice(text.as_bytes()),
                    Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                    Ok(Some(Ok(_))) => {}
                    Ok(Some(Err(e))) => return Err(e).context("shell-into websocket error"),
                    Err(_) => break, // idle/first-byte timeout: no more output expected
                }
            }
            // "No more input" sentinel (an empty `Binary` frame, never a `Close` — see
            // `crates/baud-server/src/routes/shell_into.rs`'s header doc for why a client-sent
            // `Close` races the server's own pending output). The server sends the real close.
            let _ = tx.send(Message::Binary(Vec::new().into())).await;
            while let Ok(Some(Ok(_))) = tokio::time::timeout(
                std::time::Duration::from_millis(args.idle_timeout_ms),
                rx.next(),
            )
            .await
            {}

            fmt::print(&json!({ "ok": true, "output_hex": hex_encode(&output) }), json);
            Ok(())
        }
        None => interactive(tx, rx).await,
    }
}

/// Real interactive mode: forward stdin lines to the guest console, print guest output to stdout
/// as it arrives, until stdin closes (Ctrl-D) or the server closes the connection.
async fn interactive(
    mut tx: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
    mut rx: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
) -> Result<()> {
    let mut stdin_lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            line = stdin_lines.next_line() => {
                match line? {
                    Some(mut text) => {
                        text.push('\r');
                        if tx.send(Message::Binary(text.into_bytes().into())).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        // stdin closed (Ctrl-D / a piped input's EOF) — the guest may still be
                        // mid-reaction to the last line sent (e.g. echoing it back), so this must
                        // not exit immediately: send the "no more input" sentinel (an empty
                        // `Binary` frame, never a `Close` — see
                        // `crates/baud-server/src/routes/shell_into.rs`'s header doc for why a
                        // client-sent `Close` races the server's own pending output) and keep
                        // printing whatever the server sends back until *it* closes the
                        // connection.
                        let _ = tx.send(Message::Binary(Vec::new().into())).await;
                        return drain_remaining_output(&mut rx, &mut stdout).await;
                    }
                }
            }
            msg = rx.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        stdout.write_all(&bytes).await?;
                        stdout.flush().await?;
                    }
                    Some(Ok(Message::Text(text))) => {
                        stdout.write_all(text.as_bytes()).await?;
                        stdout.flush().await?;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e).context("shell-into websocket error"),
                }
            }
        }
    }
    Ok(())
}

/// Print everything the server sends after this end has stopped sending input (stdin closed),
/// until the server closes the connection back — [`interactive`]'s tail once there is nothing
/// left to send, only output still owed.
async fn drain_remaining_output(
    rx: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    stdout: &mut tokio::io::Stdout,
) -> Result<()> {
    while let Some(msg) = rx.next().await {
        match msg {
            Ok(Message::Binary(bytes)) => {
                stdout.write_all(&bytes).await?;
                stdout.flush().await?;
            }
            Ok(Message::Text(text)) => {
                stdout.write_all(text.as_bytes()).await?;
                stdout.flush().await?;
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => return Err(e).context("shell-into websocket error"),
        }
    }
    Ok(())
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() {
        return Some(Vec::new());
    }
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
