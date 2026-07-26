// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Split-virtqueue ring parsing (virtio spec 1.1 §2.6) over real `vm-memory` — the piece
// `virtio_mmio.rs`'s own doc names as the layer above its register-only bookkeeping: walking the
// descriptor table / avail ring / used ring a negotiated `QueueRingConfig` points at. Deliberately
// device-agnostic, matching the "generic core, no workload-specifics" rule `VirtioMmioTransport`
// already follows (todo.md §8's crate-map note, enforced by `baud-packages`'s `workload_lint`):
// this module knows how to drain newly-available descriptor chains and publish filled buffers back
// to the used ring; it has no opinion on *what* bytes fill them (virtio-rng's tape-seeded entropy,
// a future virtio-blk's disk data, or anything else) — that is the caller's `fill` closure.
//
// **What this module deliberately does not do yet**: raise `InterruptStatus` or inject a real IRQ
// after publishing a used-ring entry — `virtio_mmio.rs`'s doc explains why (no in-kernel irqchip on
// this host, so which vector a `virtio_mmio.device=` IRQ resolves to is unverified). Driving
// `process_available` from a real `QueueNotify` is no longer unimplemented — `console.rs`'s
// `DeviceBus::service_virtio_rng` does exactly that, filling buffers with tape-seeded entropy bytes
// — but it is still a caller-invoked step, not something a real KVM boot loop calls automatically
// yet (that needs the same interrupt-routing investigation, plus cmdline/CLI wiring, todo.md §14).

use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend};

use crate::virtio_mmio::QueueRingConfig;

/// One descriptor-table entry's on-wire size (spec 1.1 §2.6.5 `struct virtq_desc`: `le64 addr`,
/// `le32 len`, `le16 flags`, `le16 next`).
const DESC_SIZE: u64 = 16;

/// This descriptor continues into `next` — without this flag a chain terminates here (spec 1.1
/// §2.6.5).
const VIRTQ_DESC_F_NEXT: u16 = 1;
/// The device may write into this descriptor's buffer (spec 1.1 §2.6.5) — without it, the buffer
/// is driver-supplied input the device must only read.
const VIRTQ_DESC_F_WRITE: u16 = 2;
/// This descriptor's `addr`/`len` describe an indirect descriptor table, not a data buffer (spec
/// 1.1 §2.6.5.3) — unsupported here; rejected loud rather than silently mis-parsed as data.
const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// `struct virtq_avail { le16 flags; le16 idx; le16 ring[...]; }` (spec 1.1 §2.6.6) — the `idx`
/// field's byte offset, and the fixed header length before the `ring` array starts.
const AVAIL_IDX_OFFSET: u64 = 2;
const AVAIL_HEADER_LEN: u64 = 4;

/// `struct virtq_used { le16 flags; le16 idx; struct virtq_used_elem ring[...]; }` (spec 1.1
/// §2.6.8) — the `idx` field's byte offset, the fixed header length, and one `virtq_used_elem`'s
/// size (`le32 id; le32 len;`).
const USED_IDX_OFFSET: u64 = 2;
const USED_HEADER_LEN: u64 = 4;
const USED_ELEM_SIZE: u64 = 8;

/// One descriptor in a chain, decoded from guest memory — `addr`/`len` name a guest-physical
/// buffer, `write` is [`VIRTQ_DESC_F_WRITE`] (whether the device may write into it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub write: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum VirtqueueError {
    #[error("failed accessing guest memory at a virtqueue ring structure: {0}")]
    GuestMemory(vm_memory::guest_memory::Error),
    #[error("descriptor index {0} is out of range for a queue of size {1}")]
    DescriptorOutOfRange(u16, u32),
    #[error("descriptor chain did not terminate within {0} hops (the negotiated queue size) — corrupt or malicious ring")]
    ChainTooLong(u32),
    #[error("indirect descriptors are not supported")]
    IndirectUnsupported,
}

/// A split virtqueue's live processing state: the negotiated ring addresses/size
/// ([`QueueRingConfig`]) plus this side's own free-running ring cursors. Per spec 1.1 §2.6.6/§2.6.8,
/// `avail.idx`/`used.idx` are driver-owned/device-owned counters respectively that each side reads
/// modulo the queue size to find the next ring slot — `next_avail_idx` mirrors the count of chains
/// this struct has consumed so far (compared against the driver's own `avail.idx` on each poll),
/// and `next_used_idx` is the count of entries this struct has published (the sole writer of the
/// device's `used.idx`, so tracking it locally rather than re-reading it back is correct as long as
/// nothing else writes this used ring — true for any device, which owns its used ring exclusively).
pub struct SplitVirtqueue {
    config: QueueRingConfig,
    next_avail_idx: u16,
    next_used_idx: u16,
}

impl SplitVirtqueue {
    /// A virtqueue processor for `config` — freshly constructed, so it will next look for the
    /// driver's very first available chain (`avail.idx` starting from `0`) and its own used ring
    /// starts empty. Construct a new one after every device reset (`VirtioMmioTransport`'s own
    /// `reset()` already zeroes the negotiated queue state that produces a fresh `config`).
    pub fn new(config: QueueRingConfig) -> Self {
        SplitVirtqueue { config, next_avail_idx: 0, next_used_idx: 0 }
    }

    /// The ring config this instance was constructed with — lets a caller that caches a
    /// `SplitVirtqueue` across calls (`console.rs`'s `DeviceBus::service_virtio_rng`) detect a
    /// driver re-negotiation (new addresses/size after a device reset) and rebuild rather than keep
    /// walking a stale layout.
    pub(crate) fn config(&self) -> QueueRingConfig {
        self.config
    }

    fn read_u16<M: GuestMemoryBackend>(&self, mem: &M, addr: u64) -> Result<u16, VirtqueueError> {
        let mut buf = [0u8; 2];
        mem.read_slice(&mut buf, GuestAddress(addr)).map_err(VirtqueueError::GuestMemory)?;
        Ok(u16::from_le_bytes(buf))
    }

    fn write_u16<M: GuestMemoryBackend>(&self, mem: &M, addr: u64, value: u16) -> Result<(), VirtqueueError> {
        mem.write_slice(&value.to_le_bytes(), GuestAddress(addr)).map_err(VirtqueueError::GuestMemory)
    }

    /// Read descriptor-table slot `index`, returning the decoded [`Descriptor`] plus `Some(next)`
    /// if [`VIRTQ_DESC_F_NEXT`] is set (the chain continues at descriptor index `next`).
    fn read_descriptor<M: GuestMemoryBackend>(
        &self,
        mem: &M,
        index: u16,
    ) -> Result<(Descriptor, Option<u16>), VirtqueueError> {
        if u32::from(index) >= self.config.num {
            return Err(VirtqueueError::DescriptorOutOfRange(index, self.config.num));
        }
        let addr = self.config.desc + u64::from(index) * DESC_SIZE;
        let mut raw = [0u8; DESC_SIZE as usize];
        mem.read_slice(&mut raw, GuestAddress(addr)).map_err(VirtqueueError::GuestMemory)?;
        let buf_addr = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(raw[8..12].try_into().unwrap());
        let flags = u16::from_le_bytes(raw[12..14].try_into().unwrap());
        let next = u16::from_le_bytes(raw[14..16].try_into().unwrap());
        if flags & VIRTQ_DESC_F_INDIRECT != 0 {
            return Err(VirtqueueError::IndirectUnsupported);
        }
        let descriptor = Descriptor { addr: buf_addr, len, write: flags & VIRTQ_DESC_F_WRITE != 0 };
        let next_index = (flags & VIRTQ_DESC_F_NEXT != 0).then_some(next);
        Ok((descriptor, next_index))
    }

    /// Walk one descriptor chain starting at descriptor index `head`, following [`VIRTQ_DESC_F_NEXT`]
    /// links. Bounded to at most `config.num` hops — the queue's own negotiated size is the spec's
    /// own bound on a legitimate chain's length, so a chain that hasn't terminated within that many
    /// hops is corrupt or malicious and must never be trusted further (never looped on indefinitely).
    fn read_chain<M: GuestMemoryBackend>(&self, mem: &M, head: u16) -> Result<Vec<Descriptor>, VirtqueueError> {
        let mut descriptors = Vec::new();
        let mut index = head;
        for _ in 0..=self.config.num {
            let (descriptor, next) = self.read_descriptor(mem, index)?;
            descriptors.push(descriptor);
            match next {
                Some(next_index) => index = next_index,
                None => return Ok(descriptors),
            }
        }
        Err(VirtqueueError::ChainTooLong(self.config.num))
    }

    /// Drain every descriptor chain the driver has posted since the last call (or construction),
    /// calling `fill` once per writable descriptor to produce its bytes (read-only descriptors are
    /// never touched — a device must only write where the driver marked `VIRTQ_DESC_F_WRITE`), then
    /// publishing one used-ring entry per chain recording the head descriptor index and the total
    /// bytes written across all its writable descriptors (spec 1.1 §2.6.8). Returns the number of
    /// chains processed (`0` if the driver has posted nothing new since the last call).
    pub fn process_available<M: GuestMemoryBackend>(
        &mut self,
        mem: &M,
        mut fill: impl FnMut(&mut [u8]),
    ) -> Result<u32, VirtqueueError> {
        if self.config.num == 0 {
            return Ok(0); // an unconfigured/zero-size queue has nothing to process, ever.
        }
        let driver_idx = self.read_u16(mem, self.config.driver + AVAIL_IDX_OFFSET)?;
        let mut processed = 0u32;
        while self.next_avail_idx != driver_idx {
            let slot = u32::from(self.next_avail_idx) % self.config.num;
            let ring_entry_addr = self.config.driver + AVAIL_HEADER_LEN + u64::from(slot) * 2;
            let head = self.read_u16(mem, ring_entry_addr)?;
            let chain = self.read_chain(mem, head)?;

            let mut total_written: u32 = 0;
            for descriptor in &chain {
                if !descriptor.write || descriptor.len == 0 {
                    continue;
                }
                let mut buf = vec![0u8; descriptor.len as usize];
                fill(&mut buf);
                mem.write_slice(&buf, GuestAddress(descriptor.addr)).map_err(VirtqueueError::GuestMemory)?;
                total_written += descriptor.len;
            }

            let used_slot = u32::from(self.next_used_idx) % self.config.num;
            let used_elem_addr = self.config.device + USED_HEADER_LEN + u64::from(used_slot) * USED_ELEM_SIZE;
            mem.write_slice(&u32::from(head).to_le_bytes(), GuestAddress(used_elem_addr))
                .map_err(VirtqueueError::GuestMemory)?;
            mem.write_slice(&total_written.to_le_bytes(), GuestAddress(used_elem_addr + 4))
                .map_err(VirtqueueError::GuestMemory)?;
            self.next_used_idx = self.next_used_idx.wrapping_add(1);
            self.write_u16(mem, self.config.device + USED_IDX_OFFSET, self.next_used_idx)?;

            self.next_avail_idx = self.next_avail_idx.wrapping_add(1);
            processed += 1;
        }
        Ok(processed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout;
    use vm_memory::GuestMemoryMmap;

    type GuestMemory = GuestMemoryMmap<()>;

    const DESC_BASE: u64 = 0x1000;
    const AVAIL_BASE: u64 = 0x2000;
    const USED_BASE: u64 = 0x3000;
    const BUF_BASE: u64 = 0x4000;

    fn test_guest_mem() -> GuestMemory {
        GuestMemory::from_ranges(&[(GuestAddress(0), layout::GUEST_RAM_SIZE)])
            .expect("anonymous-mmap guest memory for a unit test")
    }

    fn config(num: u32) -> QueueRingConfig {
        QueueRingConfig { num, desc: DESC_BASE, driver: AVAIL_BASE, device: USED_BASE }
    }

    fn write_descriptor(mem: &GuestMemory, index: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut raw = [0u8; 16];
        raw[0..8].copy_from_slice(&addr.to_le_bytes());
        raw[8..12].copy_from_slice(&len.to_le_bytes());
        raw[12..14].copy_from_slice(&flags.to_le_bytes());
        raw[14..16].copy_from_slice(&next.to_le_bytes());
        mem.write_slice(&raw, GuestAddress(DESC_BASE + u64::from(index) * DESC_SIZE))
            .expect("write descriptor");
    }

    fn set_avail(mem: &GuestMemory, idx: u16, ring: &[u16]) {
        mem.write_slice(&idx.to_le_bytes(), GuestAddress(AVAIL_BASE + AVAIL_IDX_OFFSET)).unwrap();
        for (slot, &head) in ring.iter().enumerate() {
            mem.write_slice(
                &head.to_le_bytes(),
                GuestAddress(AVAIL_BASE + AVAIL_HEADER_LEN + slot as u64 * 2),
            )
            .unwrap();
        }
    }

    fn used_idx(mem: &GuestMemory) -> u16 {
        let mut buf = [0u8; 2];
        mem.read_slice(&mut buf, GuestAddress(USED_BASE + USED_IDX_OFFSET)).unwrap();
        u16::from_le_bytes(buf)
    }

    fn used_elem(mem: &GuestMemory, slot: u32) -> (u32, u32) {
        let addr = USED_BASE + USED_HEADER_LEN + u64::from(slot) * USED_ELEM_SIZE;
        let mut id = [0u8; 4];
        let mut len = [0u8; 4];
        mem.read_slice(&mut id, GuestAddress(addr)).unwrap();
        mem.read_slice(&mut len, GuestAddress(addr + 4)).unwrap();
        (u32::from_le_bytes(id), u32::from_le_bytes(len))
    }

    #[test]
    fn no_new_avail_entries_processes_nothing() {
        let mem = test_guest_mem();
        set_avail(&mem, 0, &[]);
        let mut vq = SplitVirtqueue::new(config(256));
        assert_eq!(vq.process_available(&mem, |_| {}).unwrap(), 0);
        assert_eq!(used_idx(&mem), 0);
    }

    #[test]
    fn a_zero_size_queue_processes_nothing_without_reading_memory() {
        // desc/driver/device left at 0 (never a valid guest address in this crate's layout, see
        // layout::GUEST_RAM_START) — proves process_available never dereferences them when num==0.
        let mut vq = SplitVirtqueue::new(QueueRingConfig { num: 0, desc: 0, driver: 0, device: 0 });
        let mem = test_guest_mem();
        assert_eq!(vq.process_available(&mem, |_| {}).unwrap(), 0);
    }

    #[test]
    fn single_writable_descriptor_is_filled_and_published_to_the_used_ring() {
        let mem = test_guest_mem();
        write_descriptor(&mem, 0, BUF_BASE, 32, VIRTQ_DESC_F_WRITE, 0);
        set_avail(&mem, 1, &[0]);

        let mut vq = SplitVirtqueue::new(config(256));
        let processed = vq.process_available(&mem, |buf| buf.fill(0xAB)).unwrap();

        assert_eq!(processed, 1);
        let mut written = [0u8; 32];
        mem.read_slice(&mut written, GuestAddress(BUF_BASE)).unwrap();
        assert_eq!(written, [0xAB; 32]);
        assert_eq!(used_idx(&mem), 1);
        assert_eq!(used_elem(&mem, 0), (0, 32));
    }

    #[test]
    fn read_only_descriptors_are_never_written_to() {
        let mem = test_guest_mem();
        // No VIRTQ_DESC_F_WRITE: a device must treat this buffer as input-only.
        write_descriptor(&mem, 0, BUF_BASE, 16, 0, 0);
        // Poison the buffer first so a wrongful write would be observable.
        mem.write_slice(&[0x55u8; 16], GuestAddress(BUF_BASE)).unwrap();
        set_avail(&mem, 1, &[0]);

        let mut fill_calls = 0;
        let mut vq = SplitVirtqueue::new(config(256));
        let processed = vq
            .process_available(&mem, |buf| {
                fill_calls += 1;
                buf.fill(0xFF);
            })
            .unwrap();

        assert_eq!(processed, 1, "the chain is still consumed and used-ring published");
        assert_eq!(fill_calls, 0, "fill is never called for a read-only descriptor");
        let mut untouched = [0u8; 16];
        mem.read_slice(&mut untouched, GuestAddress(BUF_BASE)).unwrap();
        assert_eq!(untouched, [0x55; 16]);
        assert_eq!(used_elem(&mem, 0), (0, 0), "zero bytes written for an all-read-only chain");
    }

    #[test]
    fn chained_descriptors_are_walked_via_the_next_flag_and_summed() {
        let mem = test_guest_mem();
        // desc[0] --NEXT--> desc[1] (terminal), both writable.
        write_descriptor(&mem, 0, BUF_BASE, 8, VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT, 1);
        write_descriptor(&mem, 1, BUF_BASE + 0x100, 24, VIRTQ_DESC_F_WRITE, 0);
        set_avail(&mem, 1, &[0]);

        let mut vq = SplitVirtqueue::new(config(256));
        let mut next_byte = 0u8;
        let processed = vq
            .process_available(&mem, |buf| {
                for b in buf.iter_mut() {
                    *b = next_byte;
                    next_byte = next_byte.wrapping_add(1);
                }
            })
            .unwrap();

        assert_eq!(processed, 1);
        let mut first = [0u8; 8];
        let mut second = [0u8; 24];
        mem.read_slice(&mut first, GuestAddress(BUF_BASE)).unwrap();
        mem.read_slice(&mut second, GuestAddress(BUF_BASE + 0x100)).unwrap();
        assert_eq!(first, [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(second[0], 8, "the second descriptor's fill continues where the first left off");
        assert_eq!(used_elem(&mem, 0), (0, 32), "used length sums both descriptors in the chain");
    }

    #[test]
    fn multiple_available_chains_are_all_drained_in_one_call() {
        let mem = test_guest_mem();
        for i in 0..3u16 {
            write_descriptor(&mem, i, BUF_BASE + u64::from(i) * 0x100, 4, VIRTQ_DESC_F_WRITE, 0);
        }
        set_avail(&mem, 3, &[0, 1, 2]);

        let mut vq = SplitVirtqueue::new(config(256));
        let processed = vq.process_available(&mem, |buf| buf.fill(0x11)).unwrap();

        assert_eq!(processed, 3);
        assert_eq!(used_idx(&mem), 3);
        assert_eq!(used_elem(&mem, 0), (0, 4));
        assert_eq!(used_elem(&mem, 1), (1, 4));
        assert_eq!(used_elem(&mem, 2), (2, 4));

        // A second call with no further driver activity processes nothing new.
        assert_eq!(vq.process_available(&mem, |_| {}).unwrap(), 0);
    }

    #[test]
    fn a_non_terminating_chain_is_rejected_rather_than_looping_forever() {
        let mem = test_guest_mem();
        // desc[0] --NEXT--> desc[0]: an immediate self-loop, never terminates.
        write_descriptor(&mem, 0, BUF_BASE, 4, VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT, 0);
        set_avail(&mem, 1, &[0]);

        let mut vq = SplitVirtqueue::new(config(4));
        let err = vq.process_available(&mem, |_| {}).unwrap_err();
        assert!(matches!(err, VirtqueueError::ChainTooLong(4)));
    }

    #[test]
    fn a_descriptor_index_out_of_range_is_rejected() {
        let mem = test_guest_mem();
        set_avail(&mem, 1, &[999]); // no such descriptor in a queue of size 4
        let mut vq = SplitVirtqueue::new(config(4));
        let err = vq.process_available(&mem, |_| {}).unwrap_err();
        assert!(matches!(err, VirtqueueError::DescriptorOutOfRange(999, 4)));
    }

    #[test]
    fn an_indirect_descriptor_is_rejected_as_unsupported() {
        let mem = test_guest_mem();
        write_descriptor(&mem, 0, BUF_BASE, 16, VIRTQ_DESC_F_INDIRECT, 0);
        set_avail(&mem, 1, &[0]);
        let mut vq = SplitVirtqueue::new(config(256));
        let err = vq.process_available(&mem, |_| {}).unwrap_err();
        assert!(matches!(err, VirtqueueError::IndirectUnsupported));
    }

    /// End-to-end: a real `VirtioMmioTransport` walked through the actual driver enumeration
    /// sequence (mirroring `virtio_mmio.rs`'s own `a_full_driver_enumeration_and_queue_setup_sequence_
    /// succeeds` test), then its `queue_ring_config` feeds a `SplitVirtqueue` that processes a
    /// manually-posted chain — proving the two modules compose correctly, not just in isolation.
    #[test]
    fn transport_queue_ring_config_drives_a_real_split_virtqueue() {
        use crate::virtio_mmio::{VirtioMmioTransport, VIRTIO_STATUS_DRIVER_OK};
        use baud_vcpu::Bus;

        let mem = test_guest_mem();
        let mut transport = VirtioMmioTransport::new_rng(0xd000_0000);

        fn write_reg(t: &mut VirtioMmioTransport, base: u64, offset: u64, value: u32) {
            t.mmio_write(base + offset, &value.to_le_bytes());
        }

        const BASE: u64 = 0xd000_0000;
        write_reg(&mut transport, BASE, 0x030, 0); // QueueSel = 0
        write_reg(&mut transport, BASE, 0x038, 1); // QueueNum = 1 (a single descriptor is enough here)
        write_reg(&mut transport, BASE, 0x080, DESC_BASE as u32); // QueueDescLow
        write_reg(&mut transport, BASE, 0x090, AVAIL_BASE as u32); // QueueDriverLow
        write_reg(&mut transport, BASE, 0x0a0, USED_BASE as u32); // QueueDeviceLow
        write_reg(&mut transport, BASE, 0x044, 1); // QueueReady = 1
        write_reg(&mut transport, BASE, 0x070, VIRTIO_STATUS_DRIVER_OK);

        let config = transport.queue_ring_config(0).expect("queue 0 is ready");
        assert_eq!(config.num, 1);

        write_descriptor(&mem, 0, BUF_BASE, 16, VIRTQ_DESC_F_WRITE, 0);
        set_avail(&mem, 1, &[0]);

        let mut vq = SplitVirtqueue::new(config);
        let processed = vq.process_available(&mem, |buf| buf.fill(0x42)).unwrap();
        assert_eq!(processed, 1);
        let mut written = [0u8; 16];
        mem.read_slice(&mut written, GuestAddress(BUF_BASE)).unwrap();
        assert_eq!(written, [0x42; 16]);
    }
}
