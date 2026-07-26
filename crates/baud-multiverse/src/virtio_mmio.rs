// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// A virtio-mmio (spec 1.1 §4.2.2) *transport-layer* register block: the version-2, legacy-free
// register window every virtio-mmio device exposes — magic/version/device-id/vendor-id, feature
// negotiation, per-queue selection/sizing/ring-address registers, the status FSM, and the
// interrupt-status/ack pair. todo.md §3.8/§14 names virtio-rng as the tape-fed entropy source a
// real Linux guest's `hwrng_fillfn` needs for continuous reseeding, and §4.7 (Ubuntu, H9) will
// need a deterministic virtio-blk device on the same transport — this module is the one register
// layer both devices sit on top of, kept device-agnostic (device id/features/queue count/config
// space are constructor parameters, not hardcoded), matching the "generic core, no
// workload-specifics" rule the `workload_lint` guardrail enforces (this is baud's own transport
// code, not a Mario/NES leak).
//
// **What this module deliberately does not do** (register-level bookkeeping only, by design — see
// `crate::virtio_queue` for the layer above): it never dereferences `desc`/`driver`/`device` as
// guest-memory addresses itself, and it does not raise a real interrupt — `QueueNotify` only
// records that a notification arrived (`notify_count`/`last_notified_queue`), and
// `InterruptStatus` always reads `0`. [`Self::queue_ring_config`] hands the negotiated addresses to
// `crate::virtio_queue::SplitVirtqueue`, which walks a queue's descriptor table / avail ring / used
// ring over real `vm-memory` (todo.md §14 next-actions item 1) — that piece is now implemented and
// hardware-independently tested, but nothing yet drives it from `QueueNotify` automatically, and
// injecting a real interrupt through the exact-boundary engine (`baud_vcpu::boundary`) or a new one
// is still deferred: this host has no in-kernel irqchip (`KVM_CREATE_IRQCHIP`/`KVM_IOEVENTFD` are
// never called, `linux/mod.rs`), so which vector a `virtio_mmio.device=` IRQ number resolves to is
// unverified and needs its own investigation before that can be wired in, not stubbed here.
//
// Every register is a naturally-aligned 32-bit word (the only access width the virtio-mmio spec
// permits); this mirrors `console.rs`'s `Console::pio_read`'s own precedent for a narrower-than-
// modeled access — pad short reads with `OPEN_BUS_BYTE`, ignore excess write bytes — rather than
// treating a wrong-width access as a determinism hole, since the register window itself is fully
// modeled even if a given access to it is malformed.

use baud_vcpu::{Bus, OPEN_BUS_BYTE};

/// `"virt"` read as a little-endian `u32` — every virtio-mmio device's fixed magic value (spec
/// 1.1 §4.2.2.1).
pub const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;

/// The only version this transport implements — the legacy-free, virtio-1.0+ register layout
/// (spec 1.1 §4.2.2.1's `Version`; value `1` is the legacy interface this module does not model).
pub const VIRTIO_MMIO_VERSION: u32 = 2;

/// baud's own virtio vendor id (arbitrary per spec — no registry claims it): the hex digits read
/// "ba0d".
pub const VIRTIO_VENDOR_ID_BAUD: u32 = 0x0000_ba0d;

/// The entropy-source device id (virtio spec 1.1 §5, device id table) — the value virtio-rng
/// exposes at offset `0x008` so an unmodified guest's `virtio_rng` driver binds to it.
pub const VIRTIO_DEVICE_ID_RNG: u32 = 4;

/// `VIRTIO_F_VERSION_1` (spec 1.1 §6): bit 32 of the full 64-bit feature bitmap, offered by every
/// non-legacy virtio-mmio device. A real virtio-rng device offers no device-specific feature bits
/// (the entropy device has none defined), so this is the whole feature set `new_rng` offers.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Status register bits (virtio spec 1.1 §2.1). Writing `0` is the reset trigger, handled
/// specially rather than listed here.
pub const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
pub const VIRTIO_STATUS_DRIVER: u32 = 2;
pub const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
pub const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
pub const VIRTIO_STATUS_DEVICE_NEEDS_RESET: u32 = 64;
pub const VIRTIO_STATUS_FAILED: u32 = 128;

// Register offsets within the device's MMIO window (spec 1.1 §4.2.2 Table 4.1), version-2 subset
// (no legacy `QueuePFN`/`QueueAlign`/`GuestPageSize`).
const REG_MAGIC_VALUE: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_VENDOR_ID: u64 = 0x00c;
const REG_DEVICE_FEATURES: u64 = 0x010;
const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
const REG_DRIVER_FEATURES: u64 = 0x020;
const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
const REG_QUEUE_SEL: u64 = 0x030;
const REG_QUEUE_NUM_MAX: u64 = 0x034;
const REG_QUEUE_NUM: u64 = 0x038;
const REG_QUEUE_READY: u64 = 0x044;
const REG_QUEUE_NOTIFY: u64 = 0x050;
const REG_INTERRUPT_STATUS: u64 = 0x060;
const REG_INTERRUPT_ACK: u64 = 0x064;
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_DESC_HIGH: u64 = 0x084;
const REG_QUEUE_DRIVER_LOW: u64 = 0x090;
const REG_QUEUE_DRIVER_HIGH: u64 = 0x094;
const REG_QUEUE_DEVICE_LOW: u64 = 0x0a0;
const REG_QUEUE_DEVICE_HIGH: u64 = 0x0a4;
const REG_CONFIG_GENERATION: u64 = 0x0fc;
/// Device-specific config space starts here (spec 1.1 §4.2.2); virtio-rng defines none, so
/// `new_rng`'s transport never has anything to serve past this offset besides `OPEN_BUS_BYTE`.
const REG_CONFIG_SPACE_START: u64 = 0x100;

/// Total window size a transport claims (matches [`crate::layout::VIRTIO_MMIO_RNG_LEN`]) — every
/// register this module defines fits well inside it, with room for a future device's
/// device-specific config space past [`REG_CONFIG_SPACE_START`].
const TRANSPORT_WINDOW_LEN: u64 = 0x200;

/// Everything the transport tracks per queue: the driver-chosen size, the ready flag, and the
/// three ring-address halves — stored verbatim, not yet consumed by any ring-parsing code (see
/// this module's doc).
#[derive(Debug, Default, Clone, Copy)]
struct QueueState {
    num: u32,
    ready: bool,
    desc: u64,
    driver: u64,
    device: u64,
}

/// The negotiated ring layout for one queue, handed out by [`VirtioMmioTransport::queue_ring_config`]
/// once the driver has marked it ready — the public shape `crate::virtio_queue::SplitVirtqueue`
/// consumes to walk the actual descriptor table / avail ring / used ring over `vm-memory`. Field
/// names match spec 1.1's own terms: `desc` is the descriptor table address, `driver` is the avail
/// ring ("driver area"), `device` is the used ring ("device area").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueRingConfig {
    /// The driver-negotiated queue size (number of descriptor-table/avail-ring/used-ring slots) —
    /// spec 1.1 §2.6: every ring's size is this same value.
    pub num: u32,
    /// Guest-physical address of the descriptor table (spec 1.1 §2.6.5).
    pub desc: u64,
    /// Guest-physical address of the avail ring, spec 1.1's "driver area" (§2.6.6).
    pub driver: u64,
    /// Guest-physical address of the used ring, spec 1.1's "device area" (§2.6.8).
    pub device: u64,
}

/// A virtio-mmio v2 transport register block for one device. `Bus::mmio_read`/`mmio_write` accept
/// the *absolute* guest-physical address (matching every other [`Bus`] impl in this crate,
/// `console.rs`'s `Console`/`Cmos` included) and reject anything outside `[base, base + len)` via
/// [`Self::in_range`] — a caller (`DeviceBus`) is expected to check that first, same as the
/// existing `Console::in_range`/`Cmos::in_range` convention.
pub struct VirtioMmioTransport {
    base: u64,
    device_id: u32,
    device_features: u64,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    status: u32,
    queue_sel: u32,
    queue_num_max: u32,
    queues: Vec<QueueState>,
    interrupt_status: u32,
    notify_count: u64,
    last_notified_queue: Option<u32>,
}

impl VirtioMmioTransport {
    /// A transport for `device_id`, offering `device_features` (the full 64-bit bitmap) across
    /// `queue_count` identically-sized queues, each with a driver-visible max size of
    /// `queue_num_max` descriptors.
    pub fn new(base: u64, device_id: u32, device_features: u64, queue_count: usize, queue_num_max: u32) -> Self {
        VirtioMmioTransport {
            base,
            device_id,
            device_features,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            status: 0,
            queue_sel: 0,
            queue_num_max,
            queues: vec![QueueState::default(); queue_count],
            interrupt_status: 0,
            notify_count: 0,
            last_notified_queue: None,
        }
    }

    /// A virtio-rng transport at `base`: one queue (the entropy device's sole `requestq`, spec
    /// 1.1 §5.4), offering only [`VIRTIO_F_VERSION_1`] (the entropy device defines no
    /// device-specific feature bits), a driver-visible max queue size of 256 descriptors (ample
    /// for the small fixed-size requests a `hwrng_fillfn` reseed loop issues).
    pub fn new_rng(base: u64) -> Self {
        Self::new(base, VIRTIO_DEVICE_ID_RNG, VIRTIO_F_VERSION_1, 1, 256)
    }

    /// Whether `addr` falls inside this device's MMIO window, and if so its offset from
    /// [`Self::base`] — the same `in_range` convention `Console`/`Cmos` (`console.rs`) use so a
    /// composing `Bus` (`DeviceBus`) can dispatch without duplicating the range check.
    pub fn in_range(&self, addr: u64) -> Option<u64> {
        if addr >= self.base && addr < self.base + TRANSPORT_WINDOW_LEN {
            Some(addr - self.base)
        } else {
            None
        }
    }

    /// How many times the driver has written [`REG_QUEUE_NOTIFY`] (i.e. how many times it has
    /// signalled "descriptors are available") since construction or the last [`Self::reset`] —
    /// exposed so a test (or a future ring-processing pass) can observe that a notification
    /// arrived even though nothing yet acts on it.
    pub fn notify_count(&self) -> u64 {
        self.notify_count
    }

    /// The queue index the most recent [`REG_QUEUE_NOTIFY`] write named, or `None` if the driver
    /// has never notified.
    pub fn last_notified_queue(&self) -> Option<u32> {
        self.last_notified_queue
    }

    /// The status register's current raw value — every bit the driver has written since the last
    /// reset (see [`Self::reset`]'s doc for why this is not validated against the driver's own
    /// negotiation order: a real device does not police driver correctness, it only reacts).
    pub fn status(&self) -> u32 {
        self.status
    }

    /// The negotiated ring layout for `queue_index` — `None` until the driver has both selected a
    /// real queue (`queue_index` in range) *and* marked it ready (`REG_QUEUE_READY = 1`), since
    /// `desc`/`driver`/`device` are meaningless addresses before that (spec 1.1 §3.1.1: the driver
    /// writes them, then sets `QueueReady`, in that order). This is the handoff point to
    /// [`crate::virtio_queue::SplitVirtqueue`], which walks these addresses through `vm-memory` —
    /// this module only ever tracks them as opaque register bits, never dereferences them itself.
    pub fn queue_ring_config(&self, queue_index: u32) -> Option<QueueRingConfig> {
        let queue = self.queues.get(queue_index as usize)?;
        if !queue.ready {
            return None;
        }
        Some(QueueRingConfig { num: queue.num, desc: queue.desc, driver: queue.driver, device: queue.device })
    }

    /// Writing `0` to the status register is the driver's device-reset request (spec 1.1 §2.1:
    /// "the device MUST reset when 0 is written"). Resets every piece of *driver-negotiated*
    /// state — feature selection/acceptance, status, per-queue selection/sizing/readiness/ring
    /// addresses, and the notify counters — while leaving the device's own fixed identity
    /// (`device_id`/`device_features`/`queue_num_max`/queue *count*) untouched, since those
    /// describe what the device *is*, not what a driver has negotiated with it.
    fn reset(&mut self) {
        self.device_features_sel = 0;
        self.driver_features = 0;
        self.driver_features_sel = 0;
        self.status = 0;
        self.queue_sel = 0;
        for queue in &mut self.queues {
            *queue = QueueState::default();
        }
        self.interrupt_status = 0;
        self.notify_count = 0;
        self.last_notified_queue = None;
    }

    fn selected_queue(&self) -> Option<&QueueState> {
        self.queues.get(self.queue_sel as usize)
    }

    fn selected_queue_mut(&mut self) -> Option<&mut QueueState> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    fn read_register(&self, offset: u64) -> u32 {
        match offset {
            REG_MAGIC_VALUE => VIRTIO_MMIO_MAGIC,
            REG_VERSION => VIRTIO_MMIO_VERSION,
            REG_DEVICE_ID => self.device_id,
            REG_VENDOR_ID => VIRTIO_VENDOR_ID_BAUD,
            REG_DEVICE_FEATURES => {
                let word = self.device_features_sel;
                ((self.device_features >> (32 * (word & 1))) & 0xffff_ffff) as u32
            }
            REG_QUEUE_NUM_MAX => {
                if self.selected_queue().is_some() {
                    self.queue_num_max
                } else {
                    0 // spec: "If the returned value is zero, the queue is not available."
                }
            }
            REG_QUEUE_READY => u32::from(self.selected_queue().is_some_and(|q| q.ready)),
            REG_INTERRUPT_STATUS => self.interrupt_status,
            REG_STATUS => self.status,
            REG_QUEUE_DESC_LOW => self.selected_queue().map(|q| q.desc as u32).unwrap_or(0),
            REG_QUEUE_DESC_HIGH => self.selected_queue().map(|q| (q.desc >> 32) as u32).unwrap_or(0),
            REG_QUEUE_DRIVER_LOW => self.selected_queue().map(|q| q.driver as u32).unwrap_or(0),
            REG_QUEUE_DRIVER_HIGH => self.selected_queue().map(|q| (q.driver >> 32) as u32).unwrap_or(0),
            REG_QUEUE_DEVICE_LOW => self.selected_queue().map(|q| q.device as u32).unwrap_or(0),
            REG_QUEUE_DEVICE_HIGH => self.selected_queue().map(|q| (q.device >> 32) as u32).unwrap_or(0),
            REG_CONFIG_GENERATION => 0, // never changes: config space is empty (virtio-rng) or fixed
            // Write-only registers read as 0 (spec places no meaning on reading them); anything
            // past REG_CONFIG_SPACE_START is virtio-rng's empty device-specific config space.
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            REG_DRIVER_FEATURES => {
                let shift = 32 * (self.driver_features_sel & 1);
                let mask = 0xffff_ffffu64 << shift;
                self.driver_features = (self.driver_features & !mask) | ((u64::from(value)) << shift);
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_QUEUE_NUM => {
                if let Some(queue) = self.selected_queue_mut() {
                    queue.num = value;
                }
            }
            REG_QUEUE_READY => {
                if let Some(queue) = self.selected_queue_mut() {
                    queue.ready = value != 0;
                }
            }
            REG_QUEUE_NOTIFY => {
                self.notify_count += 1;
                self.last_notified_queue = Some(value);
            }
            REG_INTERRUPT_ACK => self.interrupt_status &= !value,
            REG_STATUS => {
                if value == 0 {
                    self.reset();
                } else {
                    self.status = value;
                }
            }
            REG_QUEUE_DESC_LOW => {
                if let Some(queue) = self.selected_queue_mut() {
                    queue.desc = (queue.desc & !0xffff_ffff) | u64::from(value);
                }
            }
            REG_QUEUE_DESC_HIGH => {
                if let Some(queue) = self.selected_queue_mut() {
                    queue.desc = (queue.desc & 0xffff_ffff) | (u64::from(value) << 32);
                }
            }
            REG_QUEUE_DRIVER_LOW => {
                if let Some(queue) = self.selected_queue_mut() {
                    queue.driver = (queue.driver & !0xffff_ffff) | u64::from(value);
                }
            }
            REG_QUEUE_DRIVER_HIGH => {
                if let Some(queue) = self.selected_queue_mut() {
                    queue.driver = (queue.driver & 0xffff_ffff) | (u64::from(value) << 32);
                }
            }
            REG_QUEUE_DEVICE_LOW => {
                if let Some(queue) = self.selected_queue_mut() {
                    queue.device = (queue.device & !0xffff_ffff) | u64::from(value);
                }
            }
            REG_QUEUE_DEVICE_HIGH => {
                if let Some(queue) = self.selected_queue_mut() {
                    queue.device = (queue.device & 0xffff_ffff) | (u64::from(value) << 32);
                }
            }
            // Read-only registers (magic/version/device-id/vendor-id/queue-num-max/config-gen)
            // and anything past the modeled window: writes are absorbed silently, matching every
            // other `Bus` impl's open-bus-write convention in this crate.
            _ => {}
        }
    }
}

impl Bus for VirtioMmioTransport {
    fn pio_read(&mut self, _port: u16, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE); // virtio-mmio has no PIO window
    }

    fn pio_write(&mut self, _port: u16, _data: &[u8]) {}

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        let Some(offset) = self.in_range(addr) else {
            data.fill(OPEN_BUS_BYTE);
            return;
        };
        let value = self.read_register(offset).to_le_bytes();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = value.get(i).copied().unwrap_or(OPEN_BUS_BYTE);
        }
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        let Some(offset) = self.in_range(addr) else {
            return;
        };
        let mut bytes = [0u8; 4];
        let n = data.len().min(4);
        bytes[..n].copy_from_slice(&data[..n]);
        self.write_register(offset, u32::from_le_bytes(bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0xd000_0000;

    fn read_reg(t: &mut VirtioMmioTransport, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        t.mmio_read(BASE + offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write_reg(t: &mut VirtioMmioTransport, offset: u64, value: u32) {
        t.mmio_write(BASE + offset, &value.to_le_bytes());
    }

    #[test]
    fn identity_registers_are_fixed_and_read_only() {
        let mut t = VirtioMmioTransport::new_rng(BASE);
        assert_eq!(read_reg(&mut t, REG_MAGIC_VALUE), VIRTIO_MMIO_MAGIC);
        assert_eq!(read_reg(&mut t, REG_VERSION), VIRTIO_MMIO_VERSION);
        assert_eq!(read_reg(&mut t, REG_DEVICE_ID), VIRTIO_DEVICE_ID_RNG);
        assert_eq!(read_reg(&mut t, REG_VENDOR_ID), VIRTIO_VENDOR_ID_BAUD);

        // Attempting to write any of them is silently absorbed, never changes the read-back value.
        write_reg(&mut t, REG_MAGIC_VALUE, 0xdead_beef);
        write_reg(&mut t, REG_DEVICE_ID, 999);
        assert_eq!(read_reg(&mut t, REG_MAGIC_VALUE), VIRTIO_MMIO_MAGIC);
        assert_eq!(read_reg(&mut t, REG_DEVICE_ID), VIRTIO_DEVICE_ID_RNG);
    }

    #[test]
    fn feature_negotiation_round_trips_both_32_bit_words() {
        let mut t = VirtioMmioTransport::new_rng(BASE);
        // Word 1 (bits 32..64) carries VIRTIO_F_VERSION_1 (bit 32); word 0 is all zero.
        write_reg(&mut t, REG_DEVICE_FEATURES_SEL, 0);
        assert_eq!(read_reg(&mut t, REG_DEVICE_FEATURES), 0);
        write_reg(&mut t, REG_DEVICE_FEATURES_SEL, 1);
        assert_eq!(read_reg(&mut t, REG_DEVICE_FEATURES), 1); // bit 0 of word 1 == bit 32 overall

        // Driver accepts exactly VIRTIO_F_VERSION_1 in word 1, nothing in word 0.
        write_reg(&mut t, REG_DRIVER_FEATURES_SEL, 0);
        write_reg(&mut t, REG_DRIVER_FEATURES, 0);
        write_reg(&mut t, REG_DRIVER_FEATURES_SEL, 1);
        write_reg(&mut t, REG_DRIVER_FEATURES, 1);
        assert_eq!(t.driver_features, VIRTIO_F_VERSION_1);
    }

    #[test]
    fn a_full_driver_enumeration_and_queue_setup_sequence_succeeds() {
        // Mirrors the real order an unmodified virtio driver's `probe` follows (spec 1.1 §3.1).
        let mut t = VirtioMmioTransport::new_rng(BASE);

        write_reg(&mut t, REG_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);
        write_reg(&mut t, REG_STATUS, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

        write_reg(&mut t, REG_DEVICE_FEATURES_SEL, 1);
        let offered = read_reg(&mut t, REG_DEVICE_FEATURES);
        write_reg(&mut t, REG_DRIVER_FEATURES_SEL, 1);
        write_reg(&mut t, REG_DRIVER_FEATURES, offered);
        write_reg(
            &mut t,
            REG_STATUS,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
        );
        assert_eq!(read_reg(&mut t, REG_STATUS) & VIRTIO_STATUS_FEATURES_OK, VIRTIO_STATUS_FEATURES_OK);

        write_reg(&mut t, REG_QUEUE_SEL, 0);
        let max = read_reg(&mut t, REG_QUEUE_NUM_MAX);
        assert_eq!(max, 256);
        write_reg(&mut t, REG_QUEUE_NUM, max);
        write_reg(&mut t, REG_QUEUE_DESC_LOW, 0x1000_0000);
        write_reg(&mut t, REG_QUEUE_DESC_HIGH, 0);
        write_reg(&mut t, REG_QUEUE_DRIVER_LOW, 0x1000_1000);
        write_reg(&mut t, REG_QUEUE_DRIVER_HIGH, 0);
        write_reg(&mut t, REG_QUEUE_DEVICE_LOW, 0x1000_2000);
        write_reg(&mut t, REG_QUEUE_DEVICE_HIGH, 0);
        write_reg(&mut t, REG_QUEUE_READY, 1);
        assert_eq!(read_reg(&mut t, REG_QUEUE_READY), 1);
        assert_eq!(t.queues[0].desc, 0x1000_0000);
        assert_eq!(t.queues[0].driver, 0x1000_1000);
        assert_eq!(t.queues[0].device, 0x1000_2000);

        write_reg(
            &mut t,
            REG_STATUS,
            VIRTIO_STATUS_ACKNOWLEDGE
                | VIRTIO_STATUS_DRIVER
                | VIRTIO_STATUS_FEATURES_OK
                | VIRTIO_STATUS_DRIVER_OK,
        );
        assert_eq!(read_reg(&mut t, REG_STATUS) & VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_DRIVER_OK);
    }

    #[test]
    fn an_unavailable_queue_index_reports_zero_max_and_ignores_writes() {
        let mut t = VirtioMmioTransport::new_rng(BASE); // exactly 1 queue: index 0
        write_reg(&mut t, REG_QUEUE_SEL, 1);
        assert_eq!(read_reg(&mut t, REG_QUEUE_NUM_MAX), 0, "no such queue");
        write_reg(&mut t, REG_QUEUE_READY, 1); // must not panic or affect queue 0
        write_reg(&mut t, REG_QUEUE_SEL, 0);
        assert_eq!(read_reg(&mut t, REG_QUEUE_READY), 0, "queue 1's write must not leak into queue 0");
    }

    #[test]
    fn queue_notify_is_recorded_but_not_yet_acted_on() {
        let mut t = VirtioMmioTransport::new_rng(BASE);
        assert_eq!(t.notify_count(), 0);
        assert_eq!(t.last_notified_queue(), None);
        write_reg(&mut t, REG_QUEUE_NOTIFY, 0);
        write_reg(&mut t, REG_QUEUE_NOTIFY, 0);
        assert_eq!(t.notify_count(), 2);
        assert_eq!(t.last_notified_queue(), Some(0));
        // No ring is processed yet, so no interrupt is ever raised by a notification.
        assert_eq!(read_reg(&mut t, REG_INTERRUPT_STATUS), 0);
    }

    #[test]
    fn writing_zero_to_status_resets_negotiated_state_but_not_device_identity() {
        let mut t = VirtioMmioTransport::new_rng(BASE);
        write_reg(&mut t, REG_STATUS, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);
        write_reg(&mut t, REG_QUEUE_SEL, 0);
        write_reg(&mut t, REG_QUEUE_READY, 1);
        write_reg(&mut t, REG_QUEUE_NOTIFY, 0);
        assert_ne!(read_reg(&mut t, REG_STATUS), 0);

        write_reg(&mut t, REG_STATUS, 0);

        assert_eq!(read_reg(&mut t, REG_STATUS), 0, "status resets");
        assert_eq!(read_reg(&mut t, REG_QUEUE_READY), 0, "queue readiness resets");
        assert_eq!(t.notify_count(), 0, "notify counters reset");
        assert_eq!(read_reg(&mut t, REG_DEVICE_ID), VIRTIO_DEVICE_ID_RNG, "device identity persists");
        write_reg(&mut t, REG_QUEUE_SEL, 0);
        assert_eq!(read_reg(&mut t, REG_QUEUE_NUM_MAX), 256, "queue count/max persists");
    }

    #[test]
    fn addresses_outside_the_window_read_as_open_bus_and_ignore_writes() {
        let mut t = VirtioMmioTransport::new_rng(BASE);
        let mut data = [0u8; 4];
        t.mmio_read(BASE - 1, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 4]);
        t.mmio_read(BASE + 0x1000, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 4]);
        t.mmio_write(BASE - 1, &[1, 2, 3, 4]); // must not panic
    }

    #[test]
    fn queue_ring_config_is_none_until_the_queue_is_marked_ready() {
        let mut t = VirtioMmioTransport::new_rng(BASE);
        assert_eq!(t.queue_ring_config(0), None, "unready queue exposes no ring config");
        assert_eq!(t.queue_ring_config(1), None, "out-of-range queue index");

        write_reg(&mut t, REG_QUEUE_SEL, 0);
        write_reg(&mut t, REG_QUEUE_NUM, 256);
        write_reg(&mut t, REG_QUEUE_DESC_LOW, 0x1000_0000);
        write_reg(&mut t, REG_QUEUE_DRIVER_LOW, 0x1000_1000);
        write_reg(&mut t, REG_QUEUE_DEVICE_LOW, 0x1000_2000);
        assert_eq!(t.queue_ring_config(0), None, "addresses alone are not enough without QueueReady");

        write_reg(&mut t, REG_QUEUE_READY, 1);
        assert_eq!(
            t.queue_ring_config(0),
            Some(QueueRingConfig { num: 256, desc: 0x1000_0000, driver: 0x1000_1000, device: 0x1000_2000 }),
        );
    }

    #[test]
    fn config_space_is_empty_for_the_entropy_device() {
        // virtio-rng defines no device-specific config fields (spec 1.1 §5.4) — every offset past
        // the transport window reads as a fixed value, never fabricated "real" config data.
        let mut t = VirtioMmioTransport::new_rng(BASE);
        assert_eq!(read_reg(&mut t, REG_CONFIG_SPACE_START), 0);
        assert_eq!(read_reg(&mut t, REG_CONFIG_GENERATION), 0);
    }
}
