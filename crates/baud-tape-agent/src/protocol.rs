// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Agent-side protocol types (thin wrappers over baud-proto).
#![allow(dead_code)]

use baud_proto::Msg;

/// Encode a protocol message for transport (CBOR).
pub fn encode(msg: &Msg) -> anyhow::Result<Vec<u8>> {
    baud_proto::encode(msg).map_err(|e| anyhow::anyhow!("encode error: {e}"))
}

/// Decode a protocol message from CBOR bytes.
pub fn decode(bytes: &[u8]) -> anyhow::Result<Msg> {
    baud_proto::decode(bytes).map_err(|e| anyhow::anyhow!("decode error: {e}"))
}
