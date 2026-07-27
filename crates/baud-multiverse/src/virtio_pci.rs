// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The virtio-pci *legacy* transport (virtio spec 1.0/1.1 Appendix "Legacy Interface" — the
// pre-1.0, MSI-X-less register layout every `virtio_pci_legacy` Linux driver still speaks): a
// second on-ramp to the same device model `virtio_mmio.rs` already exposes over MMIO, this time
// reachable the way a stock Ubuntu 18.04.1 initrd actually finds its devices (`todo.md` §4.7/§14
// item 5: the initrd carries `virtio_pci`/`virtio_blk`, never `virtio_mmio.device=`). Kept
// device-agnostic (device kind/features/queue count are constructor parameters), same
// "generic core, no workload-specifics" rule `virtio_mmio.rs`/`workload_lint` enforce.
//
// **Why a whole second transport instead of reusing `VirtioMmioTransport`**: the legacy PCI
// register layout is genuinely different, not just relocated — one packed I/O-port window
// (Host/Guest Features, a single `Queue Address` register holding a *page frame number* rather
// than three separate 64-bit desc/avail/used addresses, a fixed-width `Queue Size`, one shared
// 8-bit `Device Status`, and an ISR-status byte that *clears itself on read*) versus the
// MMIO transport's per-queue explicit-address, version-2 register set. What both transports
// *do* share, unchanged, is everything below the register layer: [`crate::virtio_queue::
// SplitVirtqueue`] only ever consumes a [`crate::virtio_mmio::QueueRingConfig`] (three ring
// addresses + a size); this module's only novel piece of virtqueue-adjacent logic is
// [`ring_layout_from_pfn`], which derives that same `QueueRingConfig` from the legacy
// interface's one `Queue Address` PFN using the standard split-ring layout every Linux
// `vring_init(..., VIRTIO_PCI_VRING_ALIGN)` call already assumes (desc table, then the avail
// ring immediately after it, then the used ring at the next 4096-byte boundary) — so once a
// transport instance's queue is marked live, `SplitVirtqueue` drives it exactly as it already
// drives a virtio-mmio queue, no new ring-walking code needed.
//
// **What this module does not do**: identify itself on the PCI bus. Legacy virtio-pci carries no
// magic/version/device-id/vendor-id registers in its I/O window at all (unlike virtio-mmio) —
// identity is entirely a PCI *configuration-space* fact (Vendor ID `0x1AF4`, Device ID
// `0x1000 + <virtio device type>`), which is `crate::pci::PciVirtioFunction`'s job, not this
// module's. This transport only starts answering PIO once a caller (`console.rs`'s `DeviceBus`)
// tells it which I/O port range it was assigned, mirroring how a real BAR is only "live" once a
// guest's PCI core has walked the sizing protocol and written a base address into it.
//
// **Also not yet done** (mirrors virtio_mmio.rs's own history: the register transport landed
// before ring-draining/interrupt-delivery wiring did): nothing drives `process_available`
// against this transport's queue yet, and no real boot's cmdline/CLI wires an instance in. This
// is the register-and-ring-address layer todo.md §14 item 5(a) named as the next H9 sub-step —
// "an actual virtio-pci transport device… so a probed device beyond the host bridge exists at
// all" — deliberately scoped the same size as `virtio_mmio.rs`'s own first landing.

use crate::virtio_mmio::QueueRingConfig;
use baud_vcpu::{Bus, OPEN_BUS_BYTE};

/// The legacy interface's queue-address register is a *page frame number*, not a byte address
/// (virtio spec, Legacy Interface §Queue Address): shift left by this many bits to recover the
/// descriptor table's real guest-physical address.
pub const VIRTIO_PCI_QUEUE_ADDR_SHIFT: u32 = 12;

/// The legacy interface's fixed virtqueue alignment (4096 bytes) — every `vring_init` call in a
/// real Linux `virtio_pci_legacy` driver hardcodes this same value; it is not negotiable.
pub const VIRTIO_PCI_VRING_ALIGN: u64 = 4096;

/// One `virtq_desc` entry's on-wire size (spec §2.6.5) — matches `virtio_queue.rs`'s own
/// `DESC_SIZE`, duplicated here (not imported) since the ring-layout math is this module's
/// concern, not that one's.
const DESC_ENTRY_SIZE: u64 = 16;

/// `struct virtq_avail`'s fixed header before its `ring[]` array starts (`flags` + `idx`, 2 bytes
/// each — spec §2.6.6).
const AVAIL_HEADER_LEN: u64 = 4;

/// ISR Status register bit 0 (legacy interface): "a virtqueue has used-ring entries the driver
/// should look at" — the legacy analog of `virtio_mmio.rs`'s `VIRTIO_MMIO_INT_VRING`.
pub const VIRTIO_PCI_ISR_QUEUE: u8 = 1;

// Register offsets within the legacy I/O BAR window (virtio spec, Legacy Interface register
// table; no MSI-X here, so device-specific config space — none defined for virtio-rng — would
// start at 0x14, right where this window ends).
const REG_HOST_FEATURES: u16 = 0x00; // 4 bytes, RO
const REG_GUEST_FEATURES: u16 = 0x04; // 4 bytes, RW
const REG_QUEUE_ADDRESS: u16 = 0x08; // 4 bytes, RW — a page frame number, not a byte address
const REG_QUEUE_SIZE: u16 = 0x0C; // 2 bytes, RO
const REG_QUEUE_SELECT: u16 = 0x0E; // 2 bytes, RW
const REG_QUEUE_NOTIFY: u16 = 0x10; // 2 bytes, effectively write-only (reads back 0)
const REG_DEVICE_STATUS: u16 = 0x12; // 1 byte, RW; 0 triggers reset
const REG_ISR_STATUS: u16 = 0x13; // 1 byte, RO — reading clears it

/// The I/O-port window size this transport claims — matches `crate::pci::VIRTIO_PCI_IO_BAR_LEN`,
/// the BAR0 size `PciVirtioFunction` advertises during PCI BAR sizing, so a probed device's
/// window and this transport's own dispatch range never disagree. Deliberately wider than the
/// `0x14` bytes of registers actually defined above (matching real hardware's own convention of a
/// power-of-two BAR size) — offsets `0x14..0x20` read/write as inert, same as any other
/// unimplemented register in this crate.
pub const VIRTIO_PCI_IO_WINDOW_LEN: u16 = 0x20;

/// The ring-layout addresses [`ring_layout_from_pfn`] derives — the same computation a real
/// `virtio_pci_legacy` driver's `vring_init(num, page, VIRTIO_PCI_VRING_ALIGN)` performs when it
/// receives one page-frame-number-addressed allocation from the kernel and lays out desc/avail/
/// used within it: the descriptor table at the base, the avail ("driver") ring immediately after
/// it, and the used ("device") ring at the next [`VIRTIO_PCI_VRING_ALIGN`]-byte boundary.
pub fn ring_layout_from_pfn(pfn: u32, num: u32) -> QueueRingConfig {
    let desc = u64::from(pfn) << VIRTIO_PCI_QUEUE_ADDR_SHIFT;
    let driver = desc + u64::from(num) * DESC_ENTRY_SIZE;
    let avail_ring_end = driver + AVAIL_HEADER_LEN + u64::from(num) * 2;
    let device = (avail_ring_end + VIRTIO_PCI_VRING_ALIGN - 1) & !(VIRTIO_PCI_VRING_ALIGN - 1);
    QueueRingConfig { num, desc, driver, device }
}

/// Everything the transport tracks per queue under the legacy interface: just the driver-written
/// page frame number (`0` means "not yet allocated / disabled" — spec: writing `0` to `Queue
/// Address` disables the queue). Unlike `virtio_mmio.rs`'s `QueueState`, there is no per-queue
/// driver-chosen size: the legacy `Queue Size` register is read-only, so every queue's size is
/// this transport's fixed `queue_num_max`.
#[derive(Debug, Default, Clone, Copy)]
struct QueueState {
    pfn: u32,
}

/// A virtio-pci legacy transport register block for one device, reachable over PIO at whatever
/// I/O port range [`Self::set_io_base`] most recently supplied — a caller (`console.rs`'s
/// `DeviceBus`) is expected to keep that synchronized with the PCI configuration-space BAR0 value
/// `crate::pci::PciVirtioFunction` tracks, the same "caller checks `in_range` first" convention
/// every other `Bus` impl in this crate follows.
pub struct VirtioPciTransport {
    io_base: Option<u16>,
    device_kind: u32,
    host_features: u32,
    guest_features: u32,
    queue_select: u16,
    queue_num_max: u16,
    queues: Vec<QueueState>,
    status: u8,
    isr_status: u8,
    notify_count: u64,
    last_notified_queue: Option<u16>,
}

impl VirtioPciTransport {
    /// A transport for `device_kind` (the virtio device-type id, spec §5's device id table — the
    /// same value `crate::pci::PciVirtioFunction` derives `0x1000 + device_kind` from), offering
    /// `host_features` (a 32-bit bitmap only — the legacy interface predates the 64-bit feature
    /// negotiation `VIRTIO_F_VERSION_1` gates) across `queue_count` identically-sized queues, each
    /// `queue_num_max` descriptors.
    pub fn new(device_kind: u32, host_features: u32, queue_count: usize, queue_num_max: u16) -> Self {
        VirtioPciTransport {
            io_base: None,
            device_kind,
            host_features,
            guest_features: 0,
            queue_select: 0,
            queue_num_max,
            queues: vec![QueueState::default(); queue_count],
            status: 0,
            isr_status: 0,
            notify_count: 0,
            last_notified_queue: None,
        }
    }

    /// A virtio-rng transport: one queue (the entropy device's sole `requestq`, spec §5.4),
    /// offering no device-specific feature bits (the entropy device defines none), a max queue
    /// size of 256 descriptors — the same parameters `VirtioMmioTransport::new_rng` uses, so the
    /// two transports expose an identical device even though their wire formats differ.
    pub fn new_rng() -> Self {
        Self::new(crate::virtio_mmio::VIRTIO_DEVICE_ID_RNG, 0, 1, 256)
    }

    /// The virtio device-type id this transport was constructed for — exposed so a caller wiring
    /// this transport up against `crate::pci::PciVirtioFunction` can assert the two agree on what
    /// device is being modeled.
    pub fn device_kind(&self) -> u32 {
        self.device_kind
    }

    /// Update the I/O port base this transport answers at — `None` while no guest has assigned a
    /// real BAR0 base yet (or after a PCI config-space write left it in the middle of the BAR
    /// sizing protocol), matching real hardware: a BAR with no valid base decodes no bus cycles at
    /// all. Called by `DeviceBus` after every PCI configuration-space write, synchronized against
    /// `PciHostBridge::virtio_io_base`.
    pub fn set_io_base(&mut self, io_base: Option<u16>) {
        self.io_base = io_base;
    }

    pub fn io_base(&self) -> Option<u16> {
        self.io_base
    }

    /// `port`'s offset within this transport's I/O window, or `None` if `port` is outside it (or
    /// no base is assigned yet at all).
    pub fn in_range(&self, port: u16) -> Option<u16> {
        let base = self.io_base?;
        let offset = i32::from(port) - i32::from(base);
        if (0..i32::from(VIRTIO_PCI_IO_WINDOW_LEN)).contains(&offset) { Some(offset as u16) } else { None }
    }

    pub fn notify_count(&self) -> u64 {
        self.notify_count
    }

    pub fn last_notified_queue(&self) -> Option<u16> {
        self.last_notified_queue
    }

    pub fn status(&self) -> u8 {
        self.status
    }

    pub fn isr_status(&self) -> u8 {
        self.isr_status
    }

    /// Set the "virtqueue has used entries" ISR bit — the legacy-interface analog of
    /// `VirtioMmioTransport::raise_used_buffer_notification`, called once a caller has actually
    /// drained new entries from a queue's used ring. Cleared only by the driver's own read of
    /// [`REG_ISR_STATUS`] (spec: "reading this register returns the reason for the interrupt and
    /// clears it"), never by this method — back-to-back notifications before the driver reads are
    /// never silently dropped, same OR-not-clear convention as the mmio transport.
    pub fn raise_used_buffer_notification(&mut self) {
        self.isr_status |= VIRTIO_PCI_ISR_QUEUE;
    }

    /// The negotiated ring layout for `queue_index`, or `None` until the driver has written a
    /// nonzero page frame number to `Queue Address` for it (spec: writing `0` disables the queue;
    /// there is no separate "ready" bit under the legacy interface, unlike virtio-mmio's
    /// `QueueReady`). This is the same handoff point to `crate::virtio_queue::SplitVirtqueue` that
    /// `VirtioMmioTransport::queue_ring_config` provides.
    pub fn queue_ring_config(&self, queue_index: u32) -> Option<QueueRingConfig> {
        let queue = self.queues.get(queue_index as usize)?;
        if queue.pfn == 0 {
            return None;
        }
        Some(ring_layout_from_pfn(queue.pfn, u32::from(self.queue_num_max)))
    }

    /// Writing `0` to `Device Status` is the driver's reset request (spec §2.1, same trigger as
    /// virtio-mmio's `Status` register) — resets driver-negotiated state (features, queue
    /// selection/PFNs, status, ISR, notify bookkeeping) while leaving device identity
    /// (`device_kind`/`host_features`/`queue_num_max`/queue count) untouched.
    fn reset(&mut self) {
        self.guest_features = 0;
        self.queue_select = 0;
        for queue in &mut self.queues {
            *queue = QueueState::default();
        }
        self.status = 0;
        self.isr_status = 0;
        self.notify_count = 0;
        self.last_notified_queue = None;
    }

    fn selected_queue(&self) -> Option<&QueueState> {
        self.queues.get(self.queue_select as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut QueueState> {
        self.queues.get_mut(self.queue_select as usize)
    }
}

impl Bus for VirtioPciTransport {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        let Some(offset) = self.in_range(port) else {
            data.fill(OPEN_BUS_BYTE);
            return;
        };
        let word: [u8; 4] = match offset {
            REG_HOST_FEATURES => self.host_features.to_le_bytes(),
            REG_GUEST_FEATURES => self.guest_features.to_le_bytes(),
            REG_QUEUE_ADDRESS => self.selected_queue().map(|q| q.pfn).unwrap_or(0).to_le_bytes(),
            REG_QUEUE_SIZE => {
                let size = if self.selected_queue().is_some() { self.queue_num_max } else { 0 };
                let b = size.to_le_bytes();
                [b[0], b[1], 0, 0]
            }
            REG_QUEUE_SELECT => {
                let b = self.queue_select.to_le_bytes();
                [b[0], b[1], 0, 0]
            }
            REG_QUEUE_NOTIFY => [0, 0, 0, 0], // effectively write-only
            REG_DEVICE_STATUS => [self.status, 0, 0, 0],
            REG_ISR_STATUS => {
                let value = self.isr_status;
                self.isr_status = 0; // spec: reading clears it
                [value, 0, 0, 0]
            }
            _ => [OPEN_BUS_BYTE; 4], // unimplemented register within the window (e.g. 0x14..0x20)
        };
        for (i, b) in data.iter_mut().enumerate() {
            *b = word.get(i).copied().unwrap_or(OPEN_BUS_BYTE);
        }
    }

    fn pio_write(&mut self, port: u16, data: &[u8]) {
        let Some(offset) = self.in_range(port) else { return };
        let mut word = [0u8; 4];
        let n = data.len().min(4);
        word[..n].copy_from_slice(&data[..n]);
        match offset {
            REG_GUEST_FEATURES => self.guest_features = u32::from_le_bytes(word),
            REG_QUEUE_ADDRESS => {
                let pfn = u32::from_le_bytes(word);
                if let Some(queue) = self.selected_queue_mut() {
                    queue.pfn = pfn;
                }
            }
            REG_QUEUE_SELECT => self.queue_select = u16::from_le_bytes([word[0], word[1]]),
            REG_QUEUE_NOTIFY => {
                self.notify_count += 1;
                self.last_notified_queue = Some(u16::from_le_bytes([word[0], word[1]]));
            }
            REG_DEVICE_STATUS => {
                if word[0] == 0 {
                    self.reset();
                } else {
                    self.status = word[0];
                }
            }
            // Read-only registers (host features, queue size, ISR status) and anything past the
            // modeled window: writes absorbed silently, matching every other `Bus` impl here.
            _ => {}
        }
    }

    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE); // this transport has no MMIO window
    }

    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u16 = 0xc000;

    fn transport() -> VirtioPciTransport {
        let mut t = VirtioPciTransport::new_rng();
        t.set_io_base(Some(BASE));
        t
    }

    fn read_reg(t: &mut VirtioPciTransport, offset: u16) -> u32 {
        let mut data = [0u8; 4];
        t.pio_read(BASE + offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write_reg(t: &mut VirtioPciTransport, offset: u16, value: u32) {
        t.pio_write(BASE + offset, &value.to_le_bytes());
    }

    #[test]
    fn ring_layout_matches_the_standard_legacy_split_ring_formula() {
        // pfn=1 -> desc at 0x1000 (page 1); num=256 descriptors (16 bytes each) puts avail right
        // after the desc table; used lands at the next 4096-byte boundary past the avail ring.
        let config = ring_layout_from_pfn(1, 256);
        assert_eq!(config.num, 256);
        assert_eq!(config.desc, 0x1000);
        assert_eq!(config.driver, 0x1000 + 256 * 16, "avail ring starts right after the desc table");
        // avail ring end = driver + 4 (header) + 256*2 (ring) = driver + 0x204; next 4096 boundary.
        let avail_end = config.driver + 4 + 256 * 2;
        let expected_used = avail_end.div_ceil(4096) * 4096;
        assert_eq!(config.device, expected_used);
        assert!(config.device >= avail_end, "used ring must never overlap the avail ring");
    }

    #[test]
    fn ring_layout_is_a_pure_function_of_pfn_and_num() {
        assert_eq!(ring_layout_from_pfn(5, 128), ring_layout_from_pfn(5, 128));
        assert_ne!(ring_layout_from_pfn(5, 128).desc, ring_layout_from_pfn(6, 128).desc);
    }

    #[test]
    fn unassigned_io_base_is_open_bus() {
        let mut t = VirtioPciTransport::new_rng();
        let mut data = [0u8; 4];
        t.pio_read(BASE, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 4], "no I/O base assigned yet: nothing should decode");
        t.pio_write(BASE, &[1, 2, 3, 4]); // must not panic
    }

    #[test]
    fn host_features_are_fixed_and_read_only() {
        let mut t = transport();
        assert_eq!(read_reg(&mut t, REG_HOST_FEATURES), 0, "virtio-rng defines no feature bits");
        write_reg(&mut t, REG_HOST_FEATURES, 0xffff_ffff);
        assert_eq!(read_reg(&mut t, REG_HOST_FEATURES), 0, "writes to a read-only register are absorbed");
    }

    #[test]
    fn guest_features_round_trip() {
        let mut t = transport();
        write_reg(&mut t, REG_GUEST_FEATURES, 0x1234);
        assert_eq!(read_reg(&mut t, REG_GUEST_FEATURES), 0x1234);
    }

    #[test]
    fn a_full_driver_enumeration_and_queue_setup_sequence_succeeds() {
        // Mirrors virtio_mmio.rs's own equivalent test, adapted to the legacy register set.
        let mut t = transport();

        write_reg(&mut t, REG_DEVICE_STATUS, 1); // ACKNOWLEDGE
        write_reg(&mut t, REG_DEVICE_STATUS, 1 | 2); // + DRIVER

        write_reg(&mut t, REG_QUEUE_SELECT, 0);
        assert_eq!(read_reg(&mut t, REG_QUEUE_SIZE), 256);
        write_reg(&mut t, REG_QUEUE_ADDRESS, 0x100); // pfn=0x100
        assert_eq!(t.queue_ring_config(0), Some(ring_layout_from_pfn(0x100, 256)));

        write_reg(&mut t, REG_DEVICE_STATUS, 1 | 2 | 4); // + DRIVER_OK
        assert_eq!(read_reg(&mut t, REG_DEVICE_STATUS), 1 | 2 | 4);
    }

    #[test]
    fn an_unavailable_queue_index_reports_zero_size_and_ignores_writes() {
        let mut t = transport(); // exactly 1 queue: index 0
        write_reg(&mut t, REG_QUEUE_SELECT, 1);
        assert_eq!(read_reg(&mut t, REG_QUEUE_SIZE), 0, "no such queue");
        write_reg(&mut t, REG_QUEUE_ADDRESS, 0x100); // must not panic or affect queue 0
        write_reg(&mut t, REG_QUEUE_SELECT, 0);
        assert_eq!(t.queue_ring_config(0), None, "queue 1's write must not leak into queue 0");
    }

    #[test]
    fn queue_ring_config_is_none_until_a_nonzero_pfn_is_written() {
        let mut t = transport();
        assert_eq!(t.queue_ring_config(0), None);
        assert_eq!(t.queue_ring_config(1), None, "out-of-range queue index");
        write_reg(&mut t, REG_QUEUE_ADDRESS, 0); // explicit 0: still disabled
        assert_eq!(t.queue_ring_config(0), None);
        write_reg(&mut t, REG_QUEUE_ADDRESS, 7);
        assert!(t.queue_ring_config(0).is_some());
    }

    #[test]
    fn queue_notify_is_recorded_but_not_yet_acted_on() {
        let mut t = transport();
        assert_eq!(t.notify_count(), 0);
        write_reg(&mut t, REG_QUEUE_NOTIFY, 0);
        write_reg(&mut t, REG_QUEUE_NOTIFY, 0);
        assert_eq!(t.notify_count(), 2);
        assert_eq!(t.last_notified_queue(), Some(0));
        assert_eq!(read_reg(&mut t, REG_QUEUE_NOTIFY), 0, "write-only: reads back 0");
    }

    #[test]
    fn isr_status_read_clears_it() {
        let mut t = transport();
        assert_eq!(read_reg(&mut t, REG_ISR_STATUS), 0);
        t.raise_used_buffer_notification();
        assert_eq!(t.isr_status(), VIRTIO_PCI_ISR_QUEUE);
        assert_eq!(read_reg(&mut t, REG_ISR_STATUS), u32::from(VIRTIO_PCI_ISR_QUEUE), "first read observes it");
        assert_eq!(read_reg(&mut t, REG_ISR_STATUS), 0, "reading clears it: a second read sees nothing");
    }

    #[test]
    fn writing_zero_to_status_resets_negotiated_state_but_not_device_identity() {
        let mut t = transport();
        write_reg(&mut t, REG_DEVICE_STATUS, 1 | 2);
        write_reg(&mut t, REG_QUEUE_SELECT, 0);
        write_reg(&mut t, REG_QUEUE_ADDRESS, 0x100);
        write_reg(&mut t, REG_QUEUE_NOTIFY, 0);
        assert_ne!(read_reg(&mut t, REG_DEVICE_STATUS), 0);

        write_reg(&mut t, REG_DEVICE_STATUS, 0);

        assert_eq!(read_reg(&mut t, REG_DEVICE_STATUS), 0, "status resets");
        assert_eq!(t.queue_ring_config(0), None, "queue PFN resets");
        assert_eq!(t.notify_count(), 0, "notify counters reset");
        write_reg(&mut t, REG_QUEUE_SELECT, 0);
        assert_eq!(read_reg(&mut t, REG_QUEUE_SIZE), 256, "queue count/max persists (device identity)");
    }

    #[test]
    fn addresses_outside_the_window_but_still_in_the_bar_read_open_bus_and_absorb_writes() {
        let mut t = transport();
        let mut data = [0u8; 4];
        t.pio_read(BASE + 0x14, &mut data); // inside the BAR (0x20 bytes) but past defined registers
        assert_eq!(data, [OPEN_BUS_BYTE; 4]);
        t.pio_write(BASE + 0x14, &[1, 2, 3, 4]); // must not panic
    }

    #[test]
    fn addresses_outside_the_whole_bar_are_open_bus() {
        let mut t = transport();
        let mut data = [0u8; 4];
        t.pio_read(BASE - 1, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 4]);
        t.pio_read(BASE + VIRTIO_PCI_IO_WINDOW_LEN, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 4]);
    }

    #[test]
    fn set_io_base_none_stops_the_transport_from_decoding_its_old_window() {
        let mut t = transport();
        write_reg(&mut t, REG_GUEST_FEATURES, 0xAB);
        t.set_io_base(None);
        let mut data = [0u8; 4];
        t.pio_read(BASE + REG_GUEST_FEATURES, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 4], "with the BAR unassigned, nothing should decode");
    }
}
