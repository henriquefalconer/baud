// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// A minimal dual-8259 (master + slave) PIC bookkeeping stub, ports 0x20/0x21 (master
// command/data) and 0xA0/0xA1 (slave command/data) -- answers the "which vector would an
// unmodified Linux guest's real virtio_mmio driver bind to" research question todo.md §14 left
// open across many prior iterations (this VMM registers no in-kernel irqchip at all --
// `KVM_CREATE_IRQCHIP`/`KVM_IOEVENTFD` are never called, confirmed by grep -- so before this
// module existed, ports 0x21/0xA1 fell through to `OpenBusFallback` and read back a fixed 0xFF).
//
// **Why this matters even though baud never lets the emulated PIC actually raise an interrupt.**
// baud always delivers interrupts itself, directly, via `KVM_INTERRUPT` at an exact instruction
// boundary (`baud_vcpu::boundary`) -- the same mechanism the LAPIC timer and the hand-assembled
// virtio-rng-guest fixture already use, and this module changes nothing about that. What it fixes
// is a *guest-side* boot precondition: Linux's `probe_8259A()` (`arch/x86/kernel/i8259.c`) writes
// a mask byte to port 0x21 and reads it back to confirm a PIC exists at all, before it will trust
// any ISA IRQ line. On a truly open-bus port (returns 0xFF unconditionally, ignores writes) that
// probe fails, `legacy_pic` falls back to `null_legacy_pic`, `nr_legacy_irqs()` returns 0,
// `early_irq_init` preallocates zero legacy IRQ descriptors, and `request_irq()` on *any* ISA IRQ
// number -- including the one `virtio_mmio.device=<size>@<base>:<irq>` names -- returns -EINVAL
// unconditionally. So an unmodified Linux guest's virtio_mmio driver can never even register its
// interrupt handler without this stub, regardless of how baud chooses to deliver the interrupt
// itself.
//
// **The vector question, answered.** Once the probe passes, `init_8259A()` performs the real
// ICW1..ICW4 handshake this module also models (both chips), and `init_IRQ`/`init_ISA_irqs`
// (`arch/x86/kernel/apic/vector.c`, `arch/x86/kernel/irqinit.c`) populate the per-CPU `vector_irq[]`
// array via `ISA_IRQ_VECTOR(irq) = ((FIRST_EXTERNAL_VECTOR + 16) & ~15) + irq = 0x30 + irq` for
// `irq` in `0..16` (`arch/x86/include/asm/irq_vectors.h`) -- **this is the CPU interrupt vector
// baud must deliver to, via its own direct `KVM_INTERRUPT` injection, for a real Linux guest's ISA
// IRQ N to reach that guest's own registered handler.** It is deliberately *not* the same as the
// hardware ICW2 "vector base" byte Linux also programs into the chip (0x20 for the master, 0x28
// for the slave) -- that value only matters to a real 8259's own interrupt-acknowledge cycle,
// which baud's stub never performs (nothing here ever raises anything; delivery is always baud's
// own direct injection). Confirmed by reading Linux 6.18.33 source at `~/wsl-kernel-src/src`
// (grep-verified, not inferred): `drivers/virtio/virtio_mmio.c`'s `vm_cmdline_set` passes the
// cmdline IRQ number straight through as an `IORESOURCE_IRQ` resource with no ACPI/DT/fwnode
// translation at all, so the cmdline number *is* the Linux virq number *is* the `irq` this
// formula takes.
//
// IRQ0 (timer, cascade-adjacent) and IRQ2 (the master->slave cascade line, never independently
// usable) are reserved by convention; a caller wiring a virtio device should pick from the
// otherwise-free legacy lines (5, 10, 11, ...).

use baud_vcpu::{Bus, OPEN_BUS_BYTE};

pub const PIC_MASTER_CMD: u16 = 0x20;
pub const PIC_MASTER_DATA: u16 = 0x21;
pub const PIC_SLAVE_CMD: u16 = 0xa0;
pub const PIC_SLAVE_DATA: u16 = 0xa1;

/// `ICW1`'s "init" bit (bit 4) -- a command-port write with this bit set (re)starts the
/// initialization sequence, exactly as a real 8259 defines it.
const ICW1_INIT: u8 = 0x10;
/// `ICW1`'s "single mode" bit (bit 1) -- set means no cascaded slave, so `ICW3` is skipped.
const ICW1_SINGLE: u8 = 0x02;
/// `ICW1`'s "ICW4 needed" bit (bit 0).
const ICW1_NEED_ICW4: u8 = 0x01;

/// The CPU interrupt vector an unmodified Linux guest's `vector_irq[]` resolves ISA IRQ `irq` to,
/// per `ISA_IRQ_VECTOR()` (`arch/x86/include/asm/irq_vectors.h`) -- see this module's doc for the
/// full derivation. Valid for `irq` in `0..16`; a caller is responsible for picking an
/// unreserved line (not 0 or 2, see this module's doc).
pub const fn isa_irq_vector(irq: u8) -> u8 {
    0x30 + irq
}

/// One chip's init-sequence position: which write on the *data* port is expected next before the
/// chip returns to normal operation (plain IMR read/write, "OCW1"). Mirrors the real 8259's own
/// internal state machine -- a command-port write with `ICW1_INIT` set always re-enters this at
/// `ExpectIcw2` from any state, matching real hardware ("`ICW1` ... resets the sequencer").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitState {
    Ready,
    ExpectIcw2,
    ExpectIcw3,
    ExpectIcw4,
}

/// One 8259 chip's bookkeeping state -- just enough to make `probe_8259A()`'s readback check and
/// `init_8259A()`'s ICW1..ICW4 handshake succeed, and to answer OCW1 (mask) reads/writes
/// correctly afterward. No IRR/ISR modeling: this chip never actually raises anything (baud
/// injects directly, see this module's doc), and a real 8259 that raises nothing keeps both
/// registers at their power-on-reset value of 0, which is what command-port reads return here
/// unconditionally -- `mask_and_ack_8259A`'s EOI writes (OCW2, command port with bits 3 and 4
/// both clear) are simply absorbed, matching that they have nothing to acknowledge.
#[derive(Debug, Clone, Copy)]
struct PicChip {
    /// The Interrupt Mask Register -- `0xff` (everything masked) at power-on/reset, matching real
    /// hardware, and every `Default::default()` construction of this struct.
    imr: u8,
    init: InitState,
    /// Whether the sequence in progress needs `ICW3` (cascade wiring) -- from `ICW1`'s `SINGLE`
    /// bit, latched at the `ICW1` write so a later `ICW4`-vs-done decision does not need it.
    expects_icw3: bool,
    /// Whether the sequence in progress needs `ICW4` -- from `ICW1`'s `NEED_ICW4` bit.
    expects_icw4: bool,
}

impl Default for PicChip {
    fn default() -> Self {
        PicChip { imr: 0xff, init: InitState::Ready, expects_icw3: false, expects_icw4: false }
    }
}

impl PicChip {
    fn cmd_write(&mut self, byte: u8) {
        if byte & ICW1_INIT != 0 {
            // ICW1: (re-)start initialization. Real hardware also clears IMR here; harmless
            // either way since init_8259A always follows with an explicit OCW1 mask write, but
            // matching the datasheet keeps this chip's state meaningful if a caller only ever
            // issues ICW1 (e.g. a probe that never completes the handshake).
            self.imr = 0x00;
            self.expects_icw3 = byte & ICW1_SINGLE == 0;
            self.expects_icw4 = byte & ICW1_NEED_ICW4 != 0;
            self.init = InitState::ExpectIcw2;
        }
        // OCW2 (EOI variants, bit 3 and bit 4 both clear) and OCW3 (register-read select, bit 3
        // set) both land here outside an init sequence -- absorbed silently, see this struct's
        // doc on why nothing needs to be tracked for either.
    }

    /// Command-port reads (OCW3 "read register" select, IRR/ISR) always return `0`: see this
    /// struct's doc -- nothing here ever has a pending or in-service line.
    fn cmd_read(&self) -> u8 {
        0
    }

    fn data_write(&mut self, byte: u8) {
        match self.init {
            InitState::ExpectIcw2 => {
                // ICW2 (vector base): not modeled -- see this module's doc on why baud's own
                // direct-injection vector is a function of the *virq*, not the hardware ICW2
                // value a real chip would use for its own interrupt-acknowledge cycle.
                self.init = if self.expects_icw3 {
                    InitState::ExpectIcw3
                } else if self.expects_icw4 {
                    InitState::ExpectIcw4
                } else {
                    InitState::Ready
                };
            }
            InitState::ExpectIcw3 => {
                self.init =
                    if self.expects_icw4 { InitState::ExpectIcw4 } else { InitState::Ready };
            }
            InitState::ExpectIcw4 => {
                self.init = InitState::Ready;
            }
            InitState::Ready => {
                // OCW1: the mask register -- this is the write `probe_8259A()`'s readback check
                // exercises, and the one `enable_8259A_irq`/`disable_8259A_irq` issue afterward.
                self.imr = byte;
            }
        }
    }

    fn data_read(&self) -> u8 {
        // Real hardware always returns IMR here regardless of init-sequence position (nothing in
        // `init_8259A()` reads mid-sequence, so this is never actually exercised there, but
        // matches the datasheet rather than inventing a distinct "undefined" value).
        self.imr
    }
}

/// The pair of chained 8259s -- see this module's doc for why this exists and what it does and
/// does not model. Hardware-independent (plain `u8` bookkeeping, no KVM/perf), same pattern as
/// [`crate::console::Cmos`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Pic8259 {
    master: PicChip,
    slave: PicChip,
}

impl Pic8259 {
    pub(crate) fn in_range(port: u16) -> bool {
        matches!(port, PIC_MASTER_CMD | PIC_MASTER_DATA | PIC_SLAVE_CMD | PIC_SLAVE_DATA)
    }

    /// The master chip's current Interrupt Mask Register -- exposed for tests/callers that want
    /// to confirm a guest's own `enable_8259A_irq(n)` call (clearing bit `n`) took effect, without
    /// needing PIO access.
    pub fn master_imr(&self) -> u8 {
        self.master.imr
    }

    /// The slave chip's current Interrupt Mask Register (see [`Self::master_imr`]).
    pub fn slave_imr(&self) -> u8 {
        self.slave.imr
    }
}

impl Bus for Pic8259 {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        debug_assert!(Self::in_range(port));
        let byte = match port {
            PIC_MASTER_CMD => self.master.cmd_read(),
            PIC_MASTER_DATA => self.master.data_read(),
            PIC_SLAVE_CMD => self.slave.cmd_read(),
            PIC_SLAVE_DATA => self.slave.data_read(),
            _ => OPEN_BUS_BYTE,
        };
        if let Some(first) = data.first_mut() {
            *first = byte;
        }
        if data.len() > 1 {
            data[1..].fill(OPEN_BUS_BYTE);
        }
    }

    fn pio_write(&mut self, port: u16, data: &[u8]) {
        debug_assert!(Self::in_range(port));
        let Some(&byte) = data.first() else { return };
        match port {
            PIC_MASTER_CMD => self.master.cmd_write(byte),
            PIC_MASTER_DATA => self.master.data_write(byte),
            PIC_SLAVE_CMD => self.slave.cmd_write(byte),
            PIC_SLAVE_DATA => self.slave.data_write(byte),
            _ => {}
        }
    }

    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE);
    }

    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_u8(bus: &mut Pic8259, port: u16, byte: u8) {
        bus.pio_write(port, &[byte]);
    }

    fn read_u8(bus: &mut Pic8259, port: u16) -> u8 {
        let mut data = [0u8; 1];
        bus.pio_read(port, &mut data);
        data[0]
    }

    #[test]
    fn isa_irq_vector_matches_the_kernels_own_formula() {
        // ISA_IRQ_VECTOR(irq) = ((0x20 + 16) & ~15) + irq = 0x30 + irq, arch/x86/include/asm/
        // irq_vectors.h -- grep-confirmed against real Linux 6.18.33 source, see this module's
        // doc.
        assert_eq!(isa_irq_vector(0), 0x30);
        assert_eq!(isa_irq_vector(5), 0x35);
        assert_eq!(isa_irq_vector(15), 0x3f);
    }

    #[test]
    fn resets_to_fully_masked_like_real_hardware() {
        let bus = Pic8259::default();
        assert_eq!(bus.master_imr(), 0xff);
        assert_eq!(bus.slave_imr(), 0xff);
    }

    #[test]
    fn probe_8259a_readback_succeeds_on_both_chips() {
        // The exact byte `arch/x86/kernel/i8259.c`'s `probe_8259A()` uses, before this module
        // existed this port fell through to `OpenBusFallback` (always 0xff, ignores writes), so
        // the readback would have observed 0xff instead of 0xfb and the probe would have failed.
        let mut bus = Pic8259::default();
        write_u8(&mut bus, PIC_MASTER_DATA, 0xfb);
        assert_eq!(read_u8(&mut bus, PIC_MASTER_DATA), 0xfb);
        write_u8(&mut bus, PIC_SLAVE_DATA, 0xfb);
        assert_eq!(read_u8(&mut bus, PIC_SLAVE_DATA), 0xfb);
    }

    #[test]
    fn full_init_8259a_handshake_leaves_the_chip_ready_for_ocw1() {
        // The exact byte sequence Linux's `init_8259A()` issues to the master: ICW1 (init,
        // cascade, ICW4 needed) -> ICW2 (vector base 0x20, unmodeled) -> ICW3 (slave on IRQ2) ->
        // ICW4 (8086 mode) -> OCW1 (mask all) -- and the equivalent slave sequence.
        let mut bus = Pic8259::default();
        write_u8(&mut bus, PIC_MASTER_CMD, 0x11); // ICW1
        write_u8(&mut bus, PIC_MASTER_DATA, 0x20); // ICW2
        write_u8(&mut bus, PIC_MASTER_DATA, 0x04); // ICW3
        write_u8(&mut bus, PIC_MASTER_DATA, 0x01); // ICW4
        write_u8(&mut bus, PIC_MASTER_DATA, 0xff); // OCW1: mask all
        assert_eq!(read_u8(&mut bus, PIC_MASTER_DATA), 0xff);

        write_u8(&mut bus, PIC_SLAVE_CMD, 0x11);
        write_u8(&mut bus, PIC_SLAVE_DATA, 0x28);
        write_u8(&mut bus, PIC_SLAVE_DATA, 0x02);
        write_u8(&mut bus, PIC_SLAVE_DATA, 0x01);
        write_u8(&mut bus, PIC_SLAVE_DATA, 0xff);
        assert_eq!(read_u8(&mut bus, PIC_SLAVE_DATA), 0xff);
    }

    #[test]
    fn unmasking_one_irq_line_clears_only_its_bit() {
        // `enable_8259A_irq(5)`'s real effect: `cached_irq_mask &= ~(1 << 5); outb(cached_irq_mask,
        // port);` -- after full init (mask-all), unmask bit 5 and confirm only that bit clears.
        let mut bus = Pic8259::default();
        for (cmd, data, icw2, icw3) in
            [(PIC_MASTER_CMD, PIC_MASTER_DATA, 0x20u8, 0x04u8), (PIC_SLAVE_CMD, PIC_SLAVE_DATA, 0x28, 0x02)]
        {
            write_u8(&mut bus, cmd, 0x11);
            write_u8(&mut bus, data, icw2);
            write_u8(&mut bus, data, icw3);
            write_u8(&mut bus, data, 0x01);
            write_u8(&mut bus, data, 0xff);
        }
        write_u8(&mut bus, PIC_MASTER_DATA, !(1u8 << 5));
        assert_eq!(bus.master_imr(), 0xdf);
        assert_eq!(bus.slave_imr(), 0xff);
    }

    #[test]
    fn single_mode_skips_icw3() {
        // ICW1 with the SINGLE bit set (bit 1): no cascaded slave, so the very next data-port
        // write after ICW2 is ICW4, not ICW3 -- confirms `expects_icw3` gating rather than always
        // consuming a fixed 4-write sequence.
        let mut bus = Pic8259::default();
        write_u8(&mut bus, PIC_MASTER_CMD, 0x13); // ICW1: init, single, ICW4 needed
        write_u8(&mut bus, PIC_MASTER_DATA, 0x20); // ICW2
        write_u8(&mut bus, PIC_MASTER_DATA, 0x01); // ICW4 (would be misparsed as ICW3 if wrong)
        write_u8(&mut bus, PIC_MASTER_DATA, 0xaa); // OCW1
        assert_eq!(bus.master_imr(), 0xaa);
    }

    #[test]
    fn command_port_eoi_writes_are_absorbed_without_disturbing_imr() {
        // OCW2 (specific/non-specific EOI, bits 3 and 4 both clear) -- `mask_and_ack_8259A`'s
        // acknowledge write. Nothing here has anything to acknowledge (see this module's doc), so
        // this must be a pure no-op on IMR.
        let mut bus = Pic8259::default();
        write_u8(&mut bus, PIC_MASTER_DATA, 0x37);
        write_u8(&mut bus, PIC_MASTER_CMD, 0x60); // specific EOI, IRQ0
        write_u8(&mut bus, PIC_MASTER_CMD, 0x20); // non-specific EOI
        assert_eq!(bus.master_imr(), 0x37);
        assert_eq!(read_u8(&mut bus, PIC_MASTER_CMD), 0);
    }

    #[test]
    fn a_command_port_write_with_the_init_bit_re_enters_initialization_from_any_state() {
        let mut bus = Pic8259::default();
        write_u8(&mut bus, PIC_MASTER_DATA, 0x12); // OCW1 while already "Ready" by default
        write_u8(&mut bus, PIC_MASTER_CMD, 0x11); // ICW1 again
        // Now back in ExpectIcw2 -- an OCW1-shaped byte here must be consumed as ICW2, not
        // misapplied as a mask write.
        write_u8(&mut bus, PIC_MASTER_DATA, 0x20);
        write_u8(&mut bus, PIC_MASTER_DATA, 0x04);
        write_u8(&mut bus, PIC_MASTER_DATA, 0x01);
        write_u8(&mut bus, PIC_MASTER_DATA, 0x55);
        assert_eq!(bus.master_imr(), 0x55);
    }
}
