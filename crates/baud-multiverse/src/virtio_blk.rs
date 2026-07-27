// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The deterministic virtio-blk device model (virtio spec 1.1 §5.2), todo.md §14 item 5(b) — the
// next H9 sub-step after the virtio-pci legacy transport (item 5(a)). Parses `virtio_blk_req`
// chains drained by `virtio_queue::SplitVirtqueue::process_available_chains` (not
// `process_available`: a block request must *read* its read-only header/write-data descriptors,
// not just fill writable ones) and services them against a [`BlockBackingStore`] — a read-only,
// content-addressed base image plus an in-memory copy-on-write overlay for guest writes
// (specs/baud-ubuntu.md §4: "guest writes are a function of the guest's own deterministic
// execution; the base stays pristine").
//
// **Why this needs no new timing primitive ("blkreplay-style, not host-I/O-return timing",
// specs/baud-ubuntu.md §4)**: the backing store is already-resident host memory (an in-process
// `Vec<u8>` plus an in-memory overlay map), so servicing a request is a synchronous memcpy, never
// real async I/O with host-dependent latency — there is no "return" to wait on in the first
// place. Completion is delivered the same way virtio-rng's already is
// (`console.rs`'s `service_virtio_blk`, `linux::Multiverse::service_virtio_blk_interrupt`): drain
// the ring synchronously, then inject an interrupt at the next reachable work-clock boundary via
// the existing exact-boundary engine (`inject_at`/`inject_timer_tick`, `period_rcb = 0`) — the
// same idiom `service_virtio_rng_interrupt` established, reused verbatim rather than reinvented.

use crate::virtio_queue::{Descriptor, VirtqueueError};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend};

/// A virtio-blk sector's fixed size (spec §5.2.3.2) — every request's `sector` field and every
/// data descriptor's length are expressed as a multiple of this.
pub const SECTOR_SIZE: u64 = 512;

/// `struct virtio_blk_req`'s header (spec §5.2.6): `le32 type`, `le32 reserved`, `le64 sector` —
/// 16 bytes, always the chain's first (read-only) descriptor.
const REQ_HEADER_LEN: usize = 16;

/// Read a sector range from the device (spec §5.2.6, `VIRTIO_BLK_T_IN`).
const VIRTIO_BLK_T_IN: u32 = 0;
/// Write a sector range to the device (`VIRTIO_BLK_T_OUT`).
const VIRTIO_BLK_T_OUT: u32 = 1;
/// Flush any buffered writes (`VIRTIO_BLK_T_FLUSH`) — a no-op here: every write already lands
/// directly in the overlay, so there is nothing buffered to flush.
const VIRTIO_BLK_T_FLUSH: u32 = 4;

/// Request completed successfully (spec §5.2.6.2).
const VIRTIO_BLK_S_OK: u8 = 0;
/// A device or driver error occurred (used here for an out-of-range or misaligned request).
const VIRTIO_BLK_S_IOERR: u8 = 1;
/// The request type is not supported by this device (e.g. `VIRTIO_BLK_T_GET_ID`, not implemented).
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

/// A read-only, content-addressed base disk image plus an in-memory, sector-granularity
/// copy-on-write overlay for guest writes. `base` is never mutated after construction — every
/// guest write only ever inserts into `overlay`, so the base image stays pristine and byte-
/// identical across every branch/replay of a run, matching `specs/baud-ubuntu.md` §4's "the base
/// stays pristine" — determinism of the disk's *content* falls straight out of the run being a
/// pure function of `(image, tape)`: the base is part of `image`, and every write is itself a
/// deterministic consequence of the guest's own execution up to that point.
pub struct BlockBackingStore {
    base: Vec<u8>,
    overlay: std::collections::HashMap<u64, [u8; SECTOR_SIZE as usize]>,
}

impl BlockBackingStore {
    /// A backing store over `base` (raw disk image bytes — e.g. the Ubuntu cloud image's qcow2
    /// converted to raw, specs/baud-ubuntu.md §4). `base.len()` need not be a multiple of
    /// [`SECTOR_SIZE`]; [`Self::capacity_sectors`] simply floors it, matching how a real disk's
    /// last partial sector (if any) would be unreachable.
    pub fn new(base: Vec<u8>) -> Self {
        BlockBackingStore { base, overlay: std::collections::HashMap::new() }
    }

    /// The disk's capacity in [`SECTOR_SIZE`]-byte sectors — published to the guest via
    /// `virtio_pci::VirtioPciTransport::new_blk`'s device-config `capacity` field.
    pub fn capacity_sectors(&self) -> u64 {
        self.base.len() as u64 / SECTOR_SIZE
    }

    fn read_sector(&self, sector: u64, out: &mut [u8; SECTOR_SIZE as usize]) {
        if let Some(overlaid) = self.overlay.get(&sector) {
            out.copy_from_slice(overlaid);
            return;
        }
        let start = (sector * SECTOR_SIZE) as usize;
        out.copy_from_slice(&self.base[start..start + SECTOR_SIZE as usize]);
    }

    fn write_sector(&mut self, sector: u64, data: &[u8; SECTOR_SIZE as usize]) {
        self.overlay.insert(sector, *data);
    }
}

/// How many whole sectors `data_descriptors` describe in total, or `None` if any descriptor's
/// length is not a multiple of [`SECTOR_SIZE`] (a malformed/unsupported request, per spec every
/// data descriptor is sector-aligned).
fn total_sectors(data_descriptors: &[Descriptor]) -> Option<u64> {
    let mut total = 0u64;
    for descriptor in data_descriptors {
        if u64::from(descriptor.len) % SECTOR_SIZE != 0 {
            return None;
        }
        total += u64::from(descriptor.len) / SECTOR_SIZE;
    }
    Some(total)
}

/// Service one drained virtio-blk request chain (`console.rs`'s `service_virtio_blk`, itself
/// `SplitVirtqueue::process_available_chains`'s `handle` callback): the standard legacy
/// virtio-blk layout is `[header (read-only, 16 bytes), 0+ data descriptors, status (writable, 1
/// byte)]` (spec §5.2.6). Returns the total bytes written into the guest — every `VIRTIO_BLK_T_IN`
/// data byte plus the trailing status byte — the value `process_available_chains` needs for the
/// used-ring entry's length field.
pub fn service_request<M: GuestMemoryBackend>(
    mem: &M,
    chain: &[Descriptor],
    store: &mut BlockBackingStore,
) -> Result<u32, VirtqueueError> {
    // A well-formed request always has at least a header and a status descriptor; anything
    // shorter is not a request this device understands and is dropped without touching memory
    // further (there is no status descriptor to report an error into either).
    if chain.len() < 2 {
        return Ok(0);
    }
    let header_desc = &chain[0];
    let status_desc = &chain[chain.len() - 1];
    let data_descriptors = &chain[1..chain.len() - 1];

    let mut header = [0u8; REQ_HEADER_LEN];
    mem.read_slice(&mut header, GuestAddress(header_desc.addr)).map_err(VirtqueueError::GuestMemory)?;
    let request_type = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());

    let status = match request_type {
        VIRTIO_BLK_T_IN | VIRTIO_BLK_T_OUT => {
            let in_range = total_sectors(data_descriptors)
                .and_then(|n| sector.checked_add(n))
                .is_some_and(|end| end <= store.capacity_sectors());
            if in_range {
                let mut current_sector = sector;
                for descriptor in data_descriptors {
                    let sectors_in_descriptor = descriptor.len / SECTOR_SIZE as u32;
                    for i in 0..sectors_in_descriptor {
                        let addr = descriptor.addr + u64::from(i) * SECTOR_SIZE;
                        let mut buf = [0u8; SECTOR_SIZE as usize];
                        if request_type == VIRTIO_BLK_T_IN {
                            store.read_sector(current_sector, &mut buf);
                            mem.write_slice(&buf, GuestAddress(addr)).map_err(VirtqueueError::GuestMemory)?;
                        } else {
                            mem.read_slice(&mut buf, GuestAddress(addr)).map_err(VirtqueueError::GuestMemory)?;
                            store.write_sector(current_sector, &buf);
                        }
                        current_sector += 1;
                    }
                }
                VIRTIO_BLK_S_OK
            } else {
                VIRTIO_BLK_S_IOERR
            }
        }
        VIRTIO_BLK_T_FLUSH => VIRTIO_BLK_S_OK,
        _ => VIRTIO_BLK_S_UNSUPP,
    };

    let mut data_bytes_written: u32 = 0;
    if request_type == VIRTIO_BLK_T_IN && status == VIRTIO_BLK_S_OK {
        data_bytes_written = data_descriptors.iter().map(|d| d.len).sum();
    }

    let status_written = if status_desc.len >= 1 {
        mem.write_slice(&[status], GuestAddress(status_desc.addr)).map_err(VirtqueueError::GuestMemory)?;
        1
    } else {
        0
    };

    Ok(data_bytes_written + status_written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout;
    use vm_memory::GuestMemoryMmap;

    type GuestMemory = GuestMemoryMmap<()>;

    const HEADER_BASE: u64 = 0x1000;
    const DATA_BASE: u64 = 0x2000;
    const STATUS_BASE: u64 = 0x3000;

    fn test_guest_mem() -> GuestMemory {
        GuestMemory::from_ranges(&[(GuestAddress(0), layout::GUEST_RAM_SIZE)])
            .expect("anonymous-mmap guest memory for a unit test")
    }

    fn write_header(mem: &GuestMemory, request_type: u32, sector: u64) {
        let mut raw = [0u8; REQ_HEADER_LEN];
        raw[0..4].copy_from_slice(&request_type.to_le_bytes());
        raw[8..16].copy_from_slice(&sector.to_le_bytes());
        mem.write_slice(&raw, GuestAddress(HEADER_BASE)).unwrap();
    }

    fn chain(data_len: u32, data_write: bool) -> Vec<Descriptor> {
        vec![
            Descriptor { addr: HEADER_BASE, len: REQ_HEADER_LEN as u32, write: false },
            Descriptor { addr: DATA_BASE, len: data_len, write: data_write },
            Descriptor { addr: STATUS_BASE, len: 1, write: true },
        ]
    }

    fn read_status(mem: &GuestMemory) -> u8 {
        let mut byte = [0u8; 1];
        mem.read_slice(&mut byte, GuestAddress(STATUS_BASE)).unwrap();
        byte[0]
    }

    fn base_image(sectors: u64) -> Vec<u8> {
        let mut image = vec![0u8; (sectors * SECTOR_SIZE) as usize];
        for (i, byte) in image.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }
        image
    }

    #[test]
    fn read_request_returns_base_image_sector_data_and_ok_status() {
        let mem = test_guest_mem();
        let mut store = BlockBackingStore::new(base_image(4));
        write_header(&mem, VIRTIO_BLK_T_IN, 1);
        let chain = chain(SECTOR_SIZE as u32, true);

        let written = service_request(&mem, &chain, &mut store).unwrap();

        assert_eq!(written, SECTOR_SIZE as u32 + 1, "one sector of data plus the status byte");
        assert_eq!(read_status(&mem), VIRTIO_BLK_S_OK);
        let mut data = vec![0u8; SECTOR_SIZE as usize];
        mem.read_slice(&mut data, GuestAddress(DATA_BASE)).unwrap();
        let expected: Vec<u8> = (SECTOR_SIZE..2 * SECTOR_SIZE).map(|i| (i % 256) as u8).collect();
        assert_eq!(data, expected, "sector 1's own bytes from the base image");
    }

    #[test]
    fn write_request_updates_the_overlay_not_the_base_image() {
        let mem = test_guest_mem();
        let mut store = BlockBackingStore::new(base_image(4));
        mem.write_slice(&[0xAB; SECTOR_SIZE as usize], GuestAddress(DATA_BASE)).unwrap();
        write_header(&mem, VIRTIO_BLK_T_OUT, 2);
        let chain = chain(SECTOR_SIZE as u32, false);

        let written = service_request(&mem, &chain, &mut store).unwrap();

        assert_eq!(written, 1, "a write request only reports the status byte, no data bytes");
        assert_eq!(read_status(&mem), VIRTIO_BLK_S_OK);
        assert_eq!(store.base[(2 * SECTOR_SIZE) as usize], 0, "the base image itself must stay pristine");
    }

    #[test]
    fn a_read_after_a_write_observes_the_overlaid_data() {
        let mem = test_guest_mem();
        let mut store = BlockBackingStore::new(base_image(4));

        mem.write_slice(&[0xCD; SECTOR_SIZE as usize], GuestAddress(DATA_BASE)).unwrap();
        write_header(&mem, VIRTIO_BLK_T_OUT, 0);
        service_request(&mem, &chain(SECTOR_SIZE as u32, false), &mut store).unwrap();

        write_header(&mem, VIRTIO_BLK_T_IN, 0);
        service_request(&mem, &chain(SECTOR_SIZE as u32, true), &mut store).unwrap();
        let mut data = vec![0u8; SECTOR_SIZE as usize];
        mem.read_slice(&mut data, GuestAddress(DATA_BASE)).unwrap();
        assert_eq!(data, vec![0xCD; SECTOR_SIZE as usize], "the read must see the just-written overlay, not the base");
    }

    #[test]
    fn an_out_of_range_sector_reports_ioerr_without_touching_memory() {
        let mem = test_guest_mem();
        let mut store = BlockBackingStore::new(base_image(2)); // only sectors 0..2 exist
        mem.write_slice(&[0x11; SECTOR_SIZE as usize], GuestAddress(DATA_BASE)).unwrap();
        write_header(&mem, VIRTIO_BLK_T_IN, 5); // out of range
        let chain = chain(SECTOR_SIZE as u32, true);

        let written = service_request(&mem, &chain, &mut store).unwrap();

        assert_eq!(written, 1, "an error report is only the status byte, no data written");
        assert_eq!(read_status(&mem), VIRTIO_BLK_S_IOERR);
        let mut untouched = vec![0u8; SECTOR_SIZE as usize];
        mem.read_slice(&mut untouched, GuestAddress(DATA_BASE)).unwrap();
        assert_eq!(untouched, vec![0x11; SECTOR_SIZE as usize], "the data buffer must be left exactly as posted");
    }

    #[test]
    fn an_unsupported_request_type_reports_unsupp() {
        let mem = test_guest_mem();
        let mut store = BlockBackingStore::new(base_image(2));
        write_header(&mem, 8, 0); // VIRTIO_BLK_T_GET_ID, not implemented
        let chain = chain(SECTOR_SIZE as u32, true);

        service_request(&mem, &chain, &mut store).unwrap();
        assert_eq!(read_status(&mem), VIRTIO_BLK_S_UNSUPP);
    }

    #[test]
    fn a_flush_request_reports_ok_and_touches_no_data() {
        let mem = test_guest_mem();
        let mut store = BlockBackingStore::new(base_image(2));
        write_header(&mem, VIRTIO_BLK_T_FLUSH, 0);
        // A flush request carries no data descriptor at all under the legacy interface: just
        // header + status.
        let chain = vec![
            Descriptor { addr: HEADER_BASE, len: REQ_HEADER_LEN as u32, write: false },
            Descriptor { addr: STATUS_BASE, len: 1, write: true },
        ];

        let written = service_request(&mem, &chain, &mut store).unwrap();
        assert_eq!(written, 1);
        assert_eq!(read_status(&mem), VIRTIO_BLK_S_OK);
    }

    #[test]
    fn a_multi_sector_request_spans_two_data_descriptors_sequentially() {
        let mem = test_guest_mem();
        let mut store = BlockBackingStore::new(base_image(4));
        const DATA_BASE_2: u64 = DATA_BASE + 0x10_000;
        write_header(&mem, VIRTIO_BLK_T_IN, 0);
        let chain = vec![
            Descriptor { addr: HEADER_BASE, len: REQ_HEADER_LEN as u32, write: false },
            Descriptor { addr: DATA_BASE, len: SECTOR_SIZE as u32, write: true },
            Descriptor { addr: DATA_BASE_2, len: SECTOR_SIZE as u32, write: true },
            Descriptor { addr: STATUS_BASE, len: 1, write: true },
        ];

        let written = service_request(&mem, &chain, &mut store).unwrap();

        assert_eq!(written, 2 * SECTOR_SIZE as u32 + 1);
        let mut first = vec![0u8; SECTOR_SIZE as usize];
        let mut second = vec![0u8; SECTOR_SIZE as usize];
        mem.read_slice(&mut first, GuestAddress(DATA_BASE)).unwrap();
        mem.read_slice(&mut second, GuestAddress(DATA_BASE_2)).unwrap();
        let expected_first: Vec<u8> = (0..SECTOR_SIZE).map(|i| (i % 256) as u8).collect();
        let expected_second: Vec<u8> = (SECTOR_SIZE..2 * SECTOR_SIZE).map(|i| (i % 256) as u8).collect();
        assert_eq!(first, expected_first, "sector 0 in the first descriptor");
        assert_eq!(second, expected_second, "sector 1 (the request's sector spans on) in the second descriptor");
    }

    #[test]
    fn capacity_sectors_matches_the_base_image_length() {
        let store = BlockBackingStore::new(base_image(7));
        assert_eq!(store.capacity_sectors(), 7);
    }

    #[test]
    fn a_chain_shorter_than_header_plus_status_is_a_harmless_no_op() {
        let mem = test_guest_mem();
        let mut store = BlockBackingStore::new(base_image(2));
        let chain = vec![Descriptor { addr: HEADER_BASE, len: REQ_HEADER_LEN as u32, write: false }];
        assert_eq!(service_request(&mem, &chain, &mut store).unwrap(), 0);
    }
}
