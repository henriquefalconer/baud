// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Universe <-> bytes: the real prerequisite todo.md §14 flagged for any SnapshotStore-backed
// persist/resume route ("nothing deserializes its bytes back into a baud_snapshot::Universe
// today" — the `/run/kvm/branch` build-status entry's "Not yet done"). [`UniverseBody`] is the
// wire-serializable projection of a [`Universe`]: every field round-trips through serde directly
// except `ram`, which is deliberately reduced to page *hashes* only, never inline page bytes
// (specs/baud-snapshot-store.md §3: "A universe body = the baud-snapshot capture, split into
// content-addressed pages ... shared across nodes via blake3 address"). A caller that wants to
// actually persist a `Universe` additionally walks [`Universe::ram_pages`] and stores each
// distinct page once (e.g. via `SnapshotStore::put_page`) — the same in-memory dedup
// `PageStore::intern` already gives capture for free, now extended across the process boundary.
//
// CBOR via `ciborium`, version-byte-prefixed — mirrors `baud_proto::encode`/`decode`'s own
// pattern (this workspace's established binary wire format for anything that isn't the plaintext
// JSON index, `baud-snapshot-store::store`'s module doc: "reusing baud-proto's own wire encoding
// rather than inventing a second one") without adding a `baud_proto::Msg` variant for a type that
// isn't a tape-device message.

use serde::{Deserialize, Serialize};

use crate::page_store::{PageHash, PageStore, PAGE_SIZE};
use crate::universe::{ClockState, DeviceState, Universe, VcpuState};

const WIRE_VERSION: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("universe body encode failed: {0}")]
    Encode(String),
    #[error("universe body decode failed: {0}")]
    Decode(String),
    #[error("empty universe body")]
    Empty,
    #[error("unsupported universe wire version {0} (expected {WIRE_VERSION})")]
    UnsupportedVersion(u8),
    #[error("page {hash}: fetch failed: {reason}")]
    PageFetchFailed { hash: String, reason: String },
    #[error("page {hash}: expected {expected} bytes, got {actual}")]
    WrongPageLength { hash: String, expected: usize, actual: usize },
    #[error("page {hash}: fetched bytes hash to a different address ({actual}) — corrupt or substituted page")]
    PageContentMismatch { hash: String, actual: String },
}

/// The wire-serializable projection of a [`Universe`] — see this module's doc comment for why
/// `ram` is hashes-only, not inline bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UniverseBody {
    pub ram_page_hashes: Vec<[u8; 32]>,
    pub vcpu: VcpuState,
    pub clock: ClockState,
    pub device: DeviceState,
    pub cpu_signature: u32,
}

impl Universe {
    /// Project this universe into its wire-serializable form. `ram` becomes page hashes only — a
    /// caller must separately persist the actual page bytes (via [`Universe::ram_pages`]) before
    /// [`encode_universe_body`]'s output is enough to reconstruct this universe elsewhere.
    pub fn to_body(&self) -> UniverseBody {
        UniverseBody {
            ram_page_hashes: self.ram.iter().map(|p| p.hash().to_bytes()).collect(),
            vcpu: self.vcpu.clone(),
            clock: self.clock.clone(),
            device: self.device.clone(),
            cpu_signature: self.cpu_signature,
        }
    }

    /// Every RAM page this universe references, in page order, paired with its content hash —
    /// what a caller persisting this universe across a process boundary hands to its own page
    /// store once per entry (duplicate hashes are cheap to persist twice: the same [`PageHash`]
    /// content-addresses to the same stored body either way, mirroring [`PageStore::intern`]'s own
    /// in-memory dedup one layer further out).
    pub fn ram_pages(&self) -> impl Iterator<Item = (PageHash, &[u8; PAGE_SIZE])> {
        self.ram.iter().map(|p| (p.hash(), p.bytes()))
    }
}

/// Encode a [`UniverseBody`] to CBOR bytes, prefixed with the version byte.
pub fn encode_universe_body(body: &UniverseBody) -> Result<Vec<u8>, WireError> {
    let mut buf = vec![WIRE_VERSION];
    ciborium::into_writer(body, &mut buf).map_err(|e| WireError::Encode(e.to_string()))?;
    Ok(buf)
}

/// Decode a [`UniverseBody`] from CBOR bytes, checking the version byte.
pub fn decode_universe_body(bytes: &[u8]) -> Result<UniverseBody, WireError> {
    let (version, rest) = bytes.split_first().ok_or(WireError::Empty)?;
    if *version != WIRE_VERSION {
        return Err(WireError::UnsupportedVersion(*version));
    }
    ciborium::from_reader(rest).map_err(|e| WireError::Decode(e.to_string()))
}

/// Rebuild a full [`Universe`] from a decoded [`UniverseBody`] plus a per-hash page fetcher (e.g.
/// a `SnapshotStore::get_page` closure) — the reverse of [`Universe::to_body`] +
/// [`Universe::ram_pages`]. Every fetched page is re-interned through `page_store` so a restored
/// universe keeps the same content-addressed sharing property a freshly captured one has (two
/// restored universes with an identical page share one allocation, not two). Rejects a fetched
/// page whose content doesn't actually hash to the address the body claims (a corrupt or
/// substituted page must never silently restore as something else).
pub fn universe_from_body(
    body: UniverseBody,
    page_store: &mut PageStore,
    mut fetch_page: impl FnMut(PageHash) -> Result<Vec<u8>, String>,
) -> Result<Universe, WireError> {
    let mut ram = Vec::with_capacity(body.ram_page_hashes.len());
    for hash_bytes in body.ram_page_hashes {
        let hash = PageHash::from_bytes(hash_bytes);
        let bytes = fetch_page(hash)
            .map_err(|reason| WireError::PageFetchFailed { hash: hash.to_hex(), reason })?;
        let page: [u8; PAGE_SIZE] = bytes.as_slice().try_into().map_err(|_| WireError::WrongPageLength {
            hash: hash.to_hex(),
            expected: PAGE_SIZE,
            actual: bytes.len(),
        })?;
        let page_ref = page_store.intern(&page);
        if page_ref.hash() != hash {
            return Err(WireError::PageContentMismatch {
                hash: hash.to_hex(),
                actual: page_ref.hash().to_hex(),
            });
        }
        ram.push(page_ref);
    }
    Ok(Universe {
        ram,
        vcpu: body.vcpu,
        clock: body.clock,
        device: body.device,
        cpu_signature: body.cpu_signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_store::PageStore;
    use crate::universe::MsrWrite;
    use std::collections::HashMap;

    fn sample_universe(store: &mut PageStore) -> Universe {
        let pages: Vec<_> = (0..4u8).map(|i| store.intern(&[i; PAGE_SIZE])).collect();
        Universe {
            ram: pages,
            vcpu: VcpuState {
                regs: vec![1, 2, 3],
                sregs: vec![4, 5],
                msrs: vec![MsrWrite { index: 0x10, data: 42 }],
                xsave: vec![9; 8],
                xcrs: vec![1],
                events: vec![2, 3],
                mp_state: vec![0],
            },
            clock: ClockState {
                kvm_clock: vec![7, 7, 7],
                tsc_khz: 3_000_000,
                work_clock_base: 12345,
                rcb_anchor: 999,
                tsc_deadline: 555,
                tsc_aux: 1,
            },
            device: DeviceState { tape_cursor: 4, console: vec![1, 2, 3, 4] },
            cpu_signature: 0x0006_5000,
        }
    }

    #[test]
    fn universe_body_roundtrips_through_cbor() {
        let mut store = PageStore::new();
        let universe = sample_universe(&mut store);
        let body = universe.to_body();
        let encoded = encode_universe_body(&body).expect("encode");
        let decoded = decode_universe_body(&encoded).expect("decode");
        assert_eq!(body, decoded);
    }

    #[test]
    fn decode_rejects_empty_input() {
        assert!(matches!(decode_universe_body(&[]), Err(WireError::Empty)));
    }

    #[test]
    fn decode_rejects_unsupported_version() {
        let bytes = vec![0xFFu8, 0, 0];
        assert!(matches!(decode_universe_body(&bytes), Err(WireError::UnsupportedVersion(0xFF))));
    }

    #[test]
    fn universe_reconstructs_from_body_and_fetched_pages() {
        let mut store = PageStore::new();
        let universe = sample_universe(&mut store);
        let body = universe.to_body();

        let mut page_bytes: HashMap<PageHash, Vec<u8>> = HashMap::new();
        for (hash, bytes) in universe.ram_pages() {
            page_bytes.insert(hash, bytes.to_vec());
        }

        let mut restore_store = PageStore::new();
        let restored = universe_from_body(body, &mut restore_store, |hash| {
            page_bytes.get(&hash).cloned().ok_or_else(|| "missing page".to_owned())
        })
        .expect("reconstruct");

        assert_eq!(restored.ram.len(), universe.ram.len());
        for (original, restored_page) in universe.ram.iter().zip(restored.ram.iter()) {
            assert_eq!(original.bytes(), restored_page.bytes());
        }
        assert_eq!(restored.vcpu, universe.vcpu);
        assert_eq!(restored.clock, universe.clock);
        assert_eq!(restored.device, universe.device);
        assert_eq!(restored.cpu_signature, universe.cpu_signature);
    }

    #[test]
    fn universe_from_body_propagates_page_fetch_error() {
        let mut store = PageStore::new();
        let universe = sample_universe(&mut store);
        let body = universe.to_body();
        let mut restore_store = PageStore::new();
        let err = universe_from_body(body, &mut restore_store, |_| Err("boom".to_owned())).unwrap_err();
        assert!(matches!(err, WireError::PageFetchFailed { .. }));
    }

    #[test]
    fn universe_from_body_detects_wrong_page_length() {
        let mut store = PageStore::new();
        let universe = sample_universe(&mut store);
        let body = universe.to_body();
        let mut restore_store = PageStore::new();
        let err = universe_from_body(body, &mut restore_store, |_| Ok(vec![0u8; 10])).unwrap_err();
        assert!(matches!(err, WireError::WrongPageLength { .. }));
    }

    #[test]
    fn universe_from_body_detects_content_mismatch() {
        let mut store = PageStore::new();
        let universe = sample_universe(&mut store);
        let body = universe.to_body();
        let mut restore_store = PageStore::new();
        // Full-size page, but the wrong content for the claimed hash.
        let err = universe_from_body(body, &mut restore_store, |_| Ok(vec![0xEEu8; PAGE_SIZE])).unwrap_err();
        assert!(matches!(err, WireError::PageContentMismatch { .. }));
    }

    #[test]
    fn ram_pages_reports_every_slot_even_when_content_repeats() {
        let mut store = PageStore::new();
        let a = store.intern(&[9u8; PAGE_SIZE]);
        let b = store.intern(&[9u8; PAGE_SIZE]);
        assert!(a.is_same_allocation(&b));
        let universe = Universe {
            ram: vec![a, b],
            vcpu: VcpuState {
                regs: vec![],
                sregs: vec![],
                msrs: vec![],
                xsave: vec![],
                xcrs: vec![],
                events: vec![],
                mp_state: vec![],
            },
            clock: ClockState {
                kvm_clock: vec![],
                tsc_khz: 0,
                work_clock_base: 0,
                rcb_anchor: 0,
                tsc_deadline: 0,
                tsc_aux: 0,
            },
            device: DeviceState { tape_cursor: 0, console: vec![] },
            cpu_signature: 0,
        };
        let body = universe.to_body();
        assert_eq!(body.ram_page_hashes.len(), 2, "one entry per RAM slot, not per distinct content");
        assert_eq!(body.ram_page_hashes[0], body.ram_page_hashes[1]);
    }
}
