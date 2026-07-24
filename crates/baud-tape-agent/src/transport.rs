// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Transport layer: WebSocket (primary) with exec/file fallback.
#![allow(dead_code)]

use anyhow::Result;
use baud_proto::Msg;

/// Transport abstraction.
pub trait Transport {
    /// Send a message to the server.
    fn send(&mut self, msg: &Msg) -> Result<()>;
    /// Receive a message from the server (returns None on EOF).
    fn recv(&mut self) -> Result<Option<Msg>>;
}

/// Stdout/stdin transport for local testing.
/// Messages are length-prefixed CBOR.
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
