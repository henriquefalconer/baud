// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// A minimal Local APIC (xAPIC) MMIO bookkeeping stub, the register window at
// `layout::LAPIC_MMIO_BASE` (`0xFEE0_0000`, Intel SDM Vol. 3A §10.4.3's conventional base) --
// todo.md §14 item 5(c)'s second flagged gap, the one `crates/baud-multiverse/src/acpi.rs`'s own
// doc named as still needed before an ACPI-enabled guest (MADT with one enabled LAPIC) could
// actually be booted against this VMM. Every existing fixture up to this point boots with
// `acpi=off` and no MADT, so the kernel's LAPIC-ID probe at this address fell through to the
// generic `OpenBusFallback` (reads back `0xFFFFFFFF`), which the kernel correctly read as "No
// local APIC present" and fell back to legacy-PIC-style interrupt handling
// (`tests/fixtures/linux-guest/BUILD.md`'s own finding). Once a guest's MADT actually advertises a
// LAPIC, `0xFFFFFFFF`/absorbed-writes is no longer a valid "device absent" signal for MMIO the way
// it is for PCI config space (`crate::pci`) -- the kernel's real `arch/x86/kernel/apic/apic.c`
// driver starts reading/writing real LAPIC registers expecting real hardware semantics, and a
// write-then-poll-for-completion loop against an always-absorbed write can spin forever.
//
// **Scope, decided by research against the real kernel source (Linux 6.18.33, `~/wsl-kernel-src/
// src`), not guessed**: a pure bookkeeping stub -- reads return the last written value (or a fixed
// read-only constant), writes are absorbed, nothing is ever computed from elapsed time -- is
// sufficient for this crate's planned boot configuration (single vCPU, `nosmp maxcpus=1`,
// `clocksource=tsc tsc=reliable`, no HPET/PIT/ACPI-PM-timer, `HW_REDUCED_ACPI`). Specifically:
// `APIC_TMCCT` (the timer's live countdown register) is read from exactly two places in the whole
// kernel tree outside KVM's own in-kernel LAPIC emulation -- `lapic_cal_handler()` (reached only
// from `calibrate_APIC_clock()`) and an `apic=debug`-only register dump -- and
// `calibrate_APIC_clock()` never reaches its polling loop here for two independent reasons: (1)
// `X86_FEATURE_TSC_DEADLINE_TIMER` is exposed by this crate's CPUID (`cpuid.rs` only clears
// RDRAND/x2APIC/RDSEED/TSX bits, bit 24 of `01H:ECX` passes through, and KVM's own CPUID synthesis
// sets it unconditionally), so `apic.c`'s `setup_boot_APIC_clock` returns early before any
// register access at all; (2) even without that, `native_calibrate_tsc()` derives
// `lapic_timer_period` arithmetically from CPUID leaf 15H (which this crate already synthesizes,
// `cpuid.rs`'s `TSC_CRYSTAL_HZ`), so `lapic_init_clockevent()` succeeds with no hardware
// measurement either way.
//
// **The one real hang hazard identified**: `APIC_ICR`'s "delivery status"/busy bit (bit 12) is
// polled unbounded by `apic_mem_wait_icr_idle()` (`arch/x86/kernel/apic/ipi.c`), reached even on a
// single vCPU via `arch_irq_work_raise()` -> `__apic_send_IPI_self()`. An open-bus `0xFFFFFFFF`
// read has that bit set and spins forever; this stub's `write_register` unconditionally clears it
// on every `ICR_LOW` write (a self-IPI is never actually "in flight" here -- baud's own direct
// `KVM_INTERRUPT` injection, unchanged, is the only interrupt-delivery mechanism this VMM ever
// uses, exactly as `Pic8259`'s own doc explains for the 8259 case), so the busy bit reads clear
// immediately and the poll loop returns on its first iteration.
//
// **Writing `MSR_IA32_TSC_DEADLINE` with no in-kernel LAPIC is a silent no-op** (KVM's own
// `kvm_set_lapic_tscdeadline_msr` short-circuits with no in-kernel LAPIC registered -- this crate
// already traps and absorbs that MSR, `timesource.rs`), not a `#GP` -- so a guest that picks
// TSC-deadline mode for its clockevent (this crate's CPUID makes that the likely choice) arms a
// deadline that silently never fires. This does not regress anything: baud has never relied on
// the guest's own clockevent arming to actually deliver a tick -- `Multiverse::run_to_first_halt_
// with_periodic_timer`'s H4 engine already delivers `LOCAL_TIMER_VECTOR` (`0xec`) directly via
// `KVM_INTERRUPT` regardless of what timer mode the guest believes it is in, the exact mechanism
// that already makes `guest_kernel_boots_to_userspace` work today with no LAPIC modeled at all.
//
// **What this module explicitly does not model**: no real interrupt is ever raised by this device
// (ISR/IRR always read all-zero -- nothing here ever marks a vector "in service" or "requested");
// SPIV/LVT-entry writes are pure bookkeeping with no side effect on interrupt delivery; TMICT/TDCR
// writes are absorbed and TMCCT mirrors whatever TMICT last held (never decrements) -- exactly
// `Pic8259`'s own "stub just enough to satisfy the probe, never a functioning device" precedent,
// not a working timer or interrupt controller. `CONFIG_ACPI=y` itself needs no other new
// MMIO/port-space device beyond this one (research finding): `acpi_reduced_hw_init()`
// (`HW_REDUCED_ACPI`, matching `crate::acpi::build_fadt`'s flag) sets `x86_init.timers.timer_init`/
// `irqs.pre_vector_init` to no-ops and `legacy_pic = &null_legacy_pic` itself, so `Pic8259`
// (`crate::pic8259`) becomes dead code for an ACPI-enabled boot specifically -- harmless, since it
// stays wired unconditionally for every other (non-ACPI) fixture, none of which this changes.

use baud_vcpu::{Bus, OPEN_BUS_BYTE};

/// The Local APIC ID register (Intel SDM Vol. 3A §10.4.6) -- read-only here, fixed at `0` in bits
/// `[31:24]` to match [`crate::acpi::build_madt`]'s sole Processor Local APIC entry (APIC ID 0),
/// the only CPU this crate ever models (`nosmp maxcpus=1`).
const REG_ID: u64 = 0x020;
/// The Local APIC Version register (Intel SDM Vol. 3A §10.4.8) -- read-only, a fixed plausible
/// "integrated APIC" value: version `0x14` (>= `0x10`, so `lapic_is_integrated()` is true) and
/// `Max LVT Entry = 6` in bits `[23:16]` (7 LVT entries total: CMCI/Timer/Thermal/PerfCount/
/// LINT0/LINT1/Error -- exactly what this stub tracks below).
const REG_LVR: u64 = 0x030;
const APIC_VERSION_VALUE: u32 = 0x0006_0014;
const REG_TPR: u64 = 0x080;
const REG_LDR: u64 = 0x0D0;
const REG_DFR: u64 = 0x0E0;
const REG_SPIV: u64 = 0x0F0;
const REG_ESR: u64 = 0x280;
const REG_ICR_LOW: u64 = 0x300;
const REG_ICR_HIGH: u64 = 0x310;
const REG_LVT_CMCI: u64 = 0x2F0;
const REG_LVT_TIMER: u64 = 0x320;
const REG_LVT_THERMAL: u64 = 0x330;
const REG_LVT_PERFCOUNT: u64 = 0x340;
const REG_LVT_LINT0: u64 = 0x350;
const REG_LVT_LINT1: u64 = 0x360;
const REG_LVT_ERROR: u64 = 0x370;
const REG_TMICT: u64 = 0x380;
const REG_TMCCT: u64 = 0x390;
const REG_TDCR: u64 = 0x3E0;

/// `APIC_ICR`'s "Delivery Status" bit (bit 12, Intel SDM Vol. 3A §10.6.1): set by real hardware
/// while an IPI is in flight, polled unbounded by `apic_mem_wait_icr_idle()`. This stub never lets
/// it read set -- see this module's own doc for why that is the one real hang hazard here.
const ICR_DELIVERY_STATUS_BIT: u32 = 1 << 12;

/// A minimal xAPIC MMIO register-window bookkeeping stub -- see this module's own doc for the full
/// scoping rationale (research-verified against the real kernel source, not guessed) and
/// `Pic8259`'s sibling doc for the general "stub just enough to satisfy the probe" precedent this
/// follows.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalApic {
    task_priority: u32,
    logical_dest: u32,
    dest_format: u32,
    spurious_interrupt_vector: u32,
    icr_low: u32,
    icr_high: u32,
    lvt_cmci: u32,
    lvt_timer: u32,
    lvt_thermal: u32,
    lvt_perfcount: u32,
    lvt_lint0: u32,
    lvt_lint1: u32,
    lvt_error: u32,
    /// The timer's initial-count register -- absorbed verbatim, never actually counted down (this
    /// is not a functioning timer; see this module's own doc for why nothing on this crate's
    /// planned boot path ever reads a live countdown).
    timer_initial_count: u32,
    timer_divide_config: u32,
}

impl LocalApic {
    /// The fixed 4 KiB xAPIC MMIO window length (Intel SDM Vol. 3A §10.4.1).
    pub const WINDOW_LEN: u64 = 0x1000;

    /// Whether `addr` falls inside [`crate::layout::LAPIC_MMIO_BASE`]'s window, and if so its
    /// offset -- the same `in_range` convention every other device on this bus follows
    /// (`Pic8259`/`VirtioMmioTransport`), so [`crate::console::DeviceBus`] can dispatch without
    /// duplicating the range check.
    pub fn in_range(addr: u64) -> Option<u64> {
        let base = crate::layout::LAPIC_MMIO_BASE;
        if addr >= base && addr < base + Self::WINDOW_LEN {
            Some(addr - base)
        } else {
            None
        }
    }

    /// The spurious-interrupt vector register's current value -- exposed for tests/callers that
    /// want to confirm a guest's own `setup_local_APIC()` (which writes the "APIC Software Enable"
    /// bit here, bit 8) took effect, mirroring `Pic8259::master_imr`'s read-access convention.
    pub fn spurious_interrupt_vector(&self) -> u32 {
        self.spurious_interrupt_vector
    }

    /// The timer LVT entry's current value -- exposed so a test can confirm the guest's own
    /// `setup_APIC_timer()` wrote the vector/mode bits it expects, without needing to poke
    /// [`Bus::mmio_read`] directly.
    pub fn lvt_timer(&self) -> u32 {
        self.lvt_timer
    }

    /// `APIC_ICR`'s low dword -- exposed for the same reason as
    /// [`Self::spurious_interrupt_vector`]; always has [`ICR_DELIVERY_STATUS_BIT`] clear (this
    /// module's own doc explains why).
    pub fn icr_low(&self) -> u32 {
        self.icr_low
    }

    fn read_register(&self, offset: u64) -> u32 {
        match offset {
            REG_ID => 0, // APIC ID 0 -- the sole CPU crate.acpi's MADT ever names
            REG_LVR => APIC_VERSION_VALUE,
            REG_TPR => self.task_priority,
            REG_LDR => self.logical_dest,
            REG_DFR => self.dest_format,
            REG_SPIV => self.spurious_interrupt_vector,
            REG_ICR_LOW => self.icr_low,
            REG_ICR_HIGH => self.icr_high,
            REG_LVT_CMCI => self.lvt_cmci,
            REG_LVT_TIMER => self.lvt_timer,
            REG_LVT_THERMAL => self.lvt_thermal,
            REG_LVT_PERFCOUNT => self.lvt_perfcount,
            REG_LVT_LINT0 => self.lvt_lint0,
            REG_LVT_LINT1 => self.lvt_lint1,
            REG_LVT_ERROR => self.lvt_error,
            REG_TMICT => self.timer_initial_count,
            // Never decrements -- not a functioning timer (this module's own doc); the value a
            // guest reads here is whatever it last armed, exactly as if no time had ever elapsed.
            REG_TMCCT => self.timer_initial_count,
            REG_TDCR => self.timer_divide_config,
            // ESR always reads 0 (this stub never generates a real APIC error); the in-service/
            // interrupt-request/trigger-mode register banks (0x100-0x170/0x180-0x1F0/0x200-0x270)
            // always read 0 (nothing here ever marks a vector in-service or requested); EOI
            // (0x0B0) and every other reserved offset in this window are architecturally
            // write-only or undefined on read -- all covered by this same fallback.
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: u64, value: u32) {
        match offset {
            REG_TPR => self.task_priority = value,
            REG_LDR => self.logical_dest = value,
            REG_DFR => self.dest_format = value,
            REG_SPIV => self.spurious_interrupt_vector = value,
            // Clear the busy bit unconditionally -- see this module's own doc: a real self-IPI is
            // never actually "in flight" here, baud's own direct `KVM_INTERRUPT` injection is the
            // only delivery mechanism this VMM ever uses, so the guest's own poll loop
            // (`apic_mem_wait_icr_idle`) must see it clear immediately, not spin forever.
            REG_ICR_LOW => self.icr_low = value & !ICR_DELIVERY_STATUS_BIT,
            REG_ICR_HIGH => self.icr_high = value,
            REG_LVT_CMCI => self.lvt_cmci = value,
            REG_LVT_TIMER => self.lvt_timer = value,
            REG_LVT_THERMAL => self.lvt_thermal = value,
            REG_LVT_PERFCOUNT => self.lvt_perfcount = value,
            REG_LVT_LINT0 => self.lvt_lint0 = value,
            REG_LVT_LINT1 => self.lvt_lint1 = value,
            REG_LVT_ERROR => self.lvt_error = value,
            REG_TMICT => self.timer_initial_count = value,
            REG_TDCR => self.timer_divide_config = value,
            // ID/LVR are read-only; EOI (0x0B0, absorbed exactly like `Pic8259`'s OCW2 EOI writes
            // -- nothing here ever raises anything, so there is nothing to acknowledge); ESR
            // (write-then-read-0 is the real protocol, already satisfied by `read_register`
            // always returning 0); ISR/IRR/TMR banks and every other reserved offset: absorbed,
            // no state to update.
            _ => {}
        }
    }
}

impl Bus for LocalApic {
    fn pio_read(&mut self, _port: u16, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE); // the LAPIC has no PIO window
    }

    fn pio_write(&mut self, _port: u16, _data: &[u8]) {}

    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        let Some(offset) = Self::in_range(addr) else {
            data.fill(OPEN_BUS_BYTE);
            return;
        };
        let value = self.read_register(offset).to_le_bytes();
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = value.get(i).copied().unwrap_or(OPEN_BUS_BYTE);
        }
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        let Some(offset) = Self::in_range(addr) else {
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

    fn read_reg(l: &mut LocalApic, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        l.mmio_read(crate::layout::LAPIC_MMIO_BASE + offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write_reg(l: &mut LocalApic, offset: u64, value: u32) {
        l.mmio_write(crate::layout::LAPIC_MMIO_BASE + offset, &value.to_le_bytes());
    }

    #[test]
    fn id_reads_zero_and_ignores_writes() {
        let mut l = LocalApic::default();
        assert_eq!(read_reg(&mut l, REG_ID), 0);
        write_reg(&mut l, REG_ID, 0xdead_beef);
        assert_eq!(read_reg(&mut l, REG_ID), 0, "ID is read-only");
    }

    #[test]
    fn version_register_reports_an_integrated_apic_with_seven_lvt_entries() {
        let mut l = LocalApic::default();
        let lvr = read_reg(&mut l, REG_LVR);
        assert!(lvr & 0xff >= 0x10, "version must read >= 0x10 for lapic_is_integrated()");
        assert_eq!((lvr >> 16) & 0xff, 6, "Max LVT Entry = 6 -- 7 LVT registers total");
    }

    #[test]
    fn isr_irr_tmr_banks_always_read_zero_and_absorb_writes() {
        let mut l = LocalApic::default();
        for base in [0x100u64, 0x180, 0x200] {
            for i in 0..8 {
                let offset = base + i * 0x10;
                write_reg(&mut l, offset, 0xffff_ffff);
                assert_eq!(read_reg(&mut l, offset), 0, "offset {offset:#x} must always read 0");
            }
        }
    }

    #[test]
    fn esr_always_reads_zero_even_after_a_write() {
        let mut l = LocalApic::default();
        write_reg(&mut l, REG_ESR, 0xffff_ffff);
        assert_eq!(read_reg(&mut l, REG_ESR), 0);
    }

    #[test]
    fn icr_busy_bit_never_reads_set_even_if_the_guest_writes_it() {
        // The one real hang hazard this module's doc names: `apic_mem_wait_icr_idle()` polls this
        // bit unbounded. A guest never legitimately sets it itself (real hardware does, on send),
        // but this proves the stub can never expose it set regardless.
        let mut l = LocalApic::default();
        write_reg(&mut l, REG_ICR_LOW, 0xffff_ffff);
        let icr_low = read_reg(&mut l, REG_ICR_LOW);
        assert_eq!(icr_low & ICR_DELIVERY_STATUS_BIT, 0, "delivery-status bit must never read set");
        assert_eq!(icr_low, !ICR_DELIVERY_STATUS_BIT, "every other bit round-trips");
    }

    #[test]
    fn icr_high_lvt_entries_ldr_dfr_tpr_spiv_all_round_trip() {
        let mut l = LocalApic::default();
        for &offset in &[
            REG_ICR_HIGH,
            REG_LVT_CMCI,
            REG_LVT_TIMER,
            REG_LVT_THERMAL,
            REG_LVT_PERFCOUNT,
            REG_LVT_LINT0,
            REG_LVT_LINT1,
            REG_LVT_ERROR,
            REG_LDR,
            REG_DFR,
            REG_TPR,
            REG_SPIV,
        ] {
            write_reg(&mut l, offset, 0x1234_5678);
            assert_eq!(read_reg(&mut l, offset), 0x1234_5678, "offset {offset:#x} must round-trip");
        }
    }

    #[test]
    fn spiv_and_lvt_timer_accessors_match_mmio_reads() {
        let mut l = LocalApic::default();
        write_reg(&mut l, REG_SPIV, 0x1ff);
        write_reg(&mut l, REG_LVT_TIMER, 0x2ec);
        assert_eq!(l.spurious_interrupt_vector(), 0x1ff);
        assert_eq!(l.lvt_timer(), 0x2ec);
        assert_eq!(l.icr_low(), 0);
    }

    #[test]
    fn timer_registers_absorb_writes_and_current_count_mirrors_initial_count_never_decrementing() {
        let mut l = LocalApic::default();
        write_reg(&mut l, REG_TMICT, 0xffff_ffff);
        write_reg(&mut l, REG_TDCR, 0x3);
        assert_eq!(read_reg(&mut l, REG_TMCCT), 0xffff_ffff, "not a functioning timer -- see module doc");
        assert_eq!(read_reg(&mut l, REG_TMICT), 0xffff_ffff);
        assert_eq!(read_reg(&mut l, REG_TDCR), 0x3);
        // A second read must be identical -- proving nothing decrements between reads.
        assert_eq!(read_reg(&mut l, REG_TMCCT), 0xffff_ffff);
    }

    #[test]
    fn eoi_write_is_absorbed_with_no_observable_state_change() {
        let mut l = LocalApic::default();
        let before = l;
        write_reg(&mut l, 0x0B0, 0xffff_ffff);
        assert_eq!(l.spurious_interrupt_vector, before.spurious_interrupt_vector);
        assert_eq!(l.lvt_timer, before.lvt_timer);
    }

    #[test]
    fn addresses_outside_the_window_are_open_bus() {
        let mut l = LocalApic::default();
        let mut data = [0u8; 4];
        l.mmio_read(crate::layout::LAPIC_MMIO_BASE + LocalApic::WINDOW_LEN, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 4]);
        assert_eq!(LocalApic::in_range(crate::layout::LAPIC_MMIO_BASE - 1), None);
        assert_eq!(LocalApic::in_range(crate::layout::LAPIC_MMIO_BASE), Some(0));
    }

    #[test]
    fn pio_reads_are_open_bus_lapic_has_no_port_window() {
        let mut l = LocalApic::default();
        let mut data = [0u8; 2];
        l.pio_read(0x20, &mut data);
        assert_eq!(data, [OPEN_BUS_BYTE; 2]);
    }
}
