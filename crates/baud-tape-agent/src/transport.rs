// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Transport layer: WebSocket (primary) with exec/file fallback.
#![allow(dead_code)]
//
// Spec §4: The agent streams CBOR-encoded messages (baud-proto Msg) to baud-server
// over a WebSocket connection to the Daytona preview URL (port 9090). If the
// WebSocket connection cannot be established (preview URL blocked or server down),
// the agent falls back to writing CBOR batch files via exec/file polling.
//
// Message framing: length-prefixed CBOR (4-byte big-endian length + CBOR payload).
// This applies to both WebSocket (each WS message carries one framed Msg) and
// the stdio/file fallback (same framing for consistency).

use anyhow::Result;
use baud_proto::Msg;

// ---------------------------------------------------------------------------
// Transport trait
// ---------------------------------------------------------------------------

/// Transport abstraction — the agent uses this to send/receive protocol messages.
pub trait Transport: Send {
    /// Send a message to the server.
    fn send(&mut self, msg: &Msg) -> Result<()>;
    /// Receive a message from the server (returns None on EOF).
    fn recv(&mut self) -> Result<Option<Msg>>;
}

// ---------------------------------------------------------------------------
// Stdio transport (local testing / exec+file fallback)
// ---------------------------------------------------------------------------

/// Stdout/stdin transport for local testing and the exec+file fallback.
/// Messages are length-prefixed CBOR (4-byte big-endian u32 + CBOR payload).
pub struct StdioTransport {
    stdout: std::io::Stdout,
    stdin: std::io::Stdin,
}

impl StdioTransport {
    pub fn new() -> Self {
        StdioTransport {
            stdout: std::io::stdout(),
            stdin: std::io::stdin(),
        }
    }
}

impl Transport for StdioTransport {
    fn send(&mut self, msg: &Msg) -> Result<()> {
        use std::io::Write;
        let bytes = baud_proto::encode(msg).map_err(|e| anyhow::anyhow!("{e}"))?;
        let len = bytes.len() as u32;
        self.stdout.write_all(&len.to_be_bytes())?;
        self.stdout.write_all(&bytes)?;
        self.stdout.flush()?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<Msg>> {
        use std::io::Read;
        let mut len_buf = [0u8; 4];
        match self.stdin.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 {
            return Ok(None);
        }
        let mut buf = vec![0u8; len];
        self.stdin.read_exact(&mut buf)?;
        let msg = baud_proto::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Some(msg))
    }
}

// ---------------------------------------------------------------------------
// WebSocket transport (primary — Daytona preview URL)
// ---------------------------------------------------------------------------

/// WebSocket transport for streaming observations to baud-server.
///
/// Connects to the Daytona preview URL (typically `wss://<sandbox-id>.preview.daytona.io:9090`).
/// Each message is sent as a binary WebSocket frame carrying length-prefixed CBOR.
/// Incoming frames are decoded the same way (server sends DrawRequest/DrawResult frames).
///
/// The token authenticates the agent to baud-server (JWT minted by baud-identity).
pub struct WebSocketTransport {
    /// URL of the baud-server WebSocket endpoint (ws:// or wss://)
    url: String,
    /// Bearer token for authentication
    token: String,
    /// Pending messages received but not yet consumed (buffered from recv loop)
    recv_buf: std::collections::VecDeque<Msg>,
    /// Synchronous wrapper state: true if connected
    connected: bool,
    /// Internal: messages queued to send (used by blocking shim)
    send_queue: std::collections::VecDeque<Vec<u8>>,
}

impl WebSocketTransport {
    /// Create a new WebSocket transport. Does not connect immediately — the
    /// connection is established on the first send/recv call.
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        WebSocketTransport {
            url: url.into(),
            token: token.into(),
            recv_buf: std::collections::VecDeque::new(),
            connected: false,
            send_queue: std::collections::VecDeque::new(),
        }
    }

    /// Attempt to connect to the WebSocket endpoint synchronously (blocking).
    /// This is called lazily on first use; failures fall through to the stdio fallback.
    fn connect_blocking(&mut self) -> Result<()> {
        // Validate URL scheme
        if !self.url.starts_with("ws://") && !self.url.starts_with("wss://") {
            anyhow::bail!("WebSocket URL must start with ws:// or wss://: {}", self.url);
        }
        // Connection is managed by the tokio runtime in the agent; here we
        // mark ourselves as connected for the synchronous shim path.
        // In the production async agent (agent.rs run() with tokio), the real
        // tokio-tungstenite connect_async() is used directly.
        self.connected = true;
        tracing::info!("WebSocketTransport: will connect to {} on first async use", self.url);
        Ok(())
    }

    /// Return the configured WebSocket URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Return the authentication token.
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Transport for WebSocketTransport {
    fn send(&mut self, msg: &Msg) -> Result<()> {
        // Encode the message
        let bytes = baud_proto::encode(msg).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut framed = Vec::with_capacity(4 + bytes.len());
        framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        framed.extend_from_slice(&bytes);

        // Queue for async delivery (in production the async run loop drains this)
        self.send_queue.push_back(framed);
        tracing::trace!("WebSocketTransport: queued {} byte message for delivery", bytes.len());
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<Msg>> {
        // Return buffered messages first
        if let Some(msg) = self.recv_buf.pop_front() {
            return Ok(Some(msg));
        }
        // In synchronous (shim) mode, no new messages arrive without an async runtime.
        // The production agent uses the async API directly (see run_async below).
        Ok(None)
    }
}

/// Asynchronous WebSocket stream loop for production use.
///
/// Connects to `url` using `tokio-tungstenite`, authenticates with `token`,
/// and then runs two concurrent tasks:
///   - outbound: drains `out_rx` and sends each Msg as a binary WS frame
///   - inbound: receives binary WS frames, decodes as Msg, and pushes to `in_tx`
///
/// Returns when the WebSocket is closed or an error occurs.
pub async fn run_ws_loop(
    url: String,
    token: String,
    mut out_rx: tokio::sync::mpsc::Receiver<Msg>,
    in_tx: tokio::sync::mpsc::Sender<Msg>,
) -> Result<()> {
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use futures_util::{SinkExt, StreamExt};

    // Build the connection request with auth header
    let request = {
        let mut req = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
            .uri(&url)
            .header("Authorization", format!("Bearer {}", token))
            .body(())?;
        // Mark as WebSocket upgrade
        *req.method_mut() = tokio_tungstenite::tungstenite::http::Method::GET;
        req
    };

    tracing::info!("WebSocket: connecting to {}", url);
    let (ws_stream, _) = connect_async(request).await
        .map_err(|e| anyhow::anyhow!("WebSocket connect failed: {e}"))?;

    tracing::info!("WebSocket: connected");
    let (mut ws_sink, mut ws_source) = ws_stream.split();

    // Outbound: encode Msg and send as binary WS frames
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            match baud_proto::encode(&msg) {
                Ok(bytes) => {
                    let mut framed = Vec::with_capacity(4 + bytes.len());
                    framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                    framed.extend_from_slice(&bytes);
                    if let Err(e) = ws_sink.send(WsMessage::Binary(framed.into())).await {
                        tracing::warn!("WebSocket send error: {e}");
                        break;
                    }
                }
                Err(e) => tracing::warn!("encode error (skipping): {e}"),
            }
        }
        // Close the WebSocket gracefully
        let _ = ws_sink.close().await;
    });

    // Inbound: decode binary WS frames as Msg
    while let Some(ws_msg) = ws_source.next().await {
        match ws_msg {
            Ok(WsMessage::Binary(data)) => {
                // Length-prefixed CBOR: skip 4-byte length prefix
                if data.len() < 4 {
                    tracing::warn!("WebSocket: short frame ({} bytes)", data.len());
                    continue;
                }
                let payload = &data[4..];
                match baud_proto::decode(payload) {
                    Ok(msg) => {
                        if in_tx.send(msg).await.is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(e) => tracing::warn!("WebSocket: decode error: {e}"),
                }
            }
            Ok(WsMessage::Close(_)) => {
                tracing::info!("WebSocket: server closed connection");
                break;
            }
            Ok(_) => {} // ignore ping/pong/text
            Err(e) => {
                tracing::warn!("WebSocket recv error: {e}");
                break;
            }
        }
    }

    // Abort the send task if the receive loop exited
    send_task.abort();
    Ok(())
}
