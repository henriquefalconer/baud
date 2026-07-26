// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Hardware-independent tests for the exit-dispatch core (specs/baud-vcpu.md §6). None of these
// touch KVM; they exercise `dispatch_exit` directly against fake `Bus`/`TimeSource`
// implementations, which is exactly what makes the dispatch logic testable on this Windows dev
// machine with no `/dev/kvm`.

use super::*;
use proptest::prelude::*;

/// A `Bus` that records every call so tests can assert routing (which port/addr, what bytes).
#[derive(Default)]
struct RecordingBus {
    pio_reads: Vec<(u16, usize)>,
    pio_writes: Vec<(u16, Vec<u8>)>,
    mmio_reads: Vec<(u64, usize)>,
    mmio_writes: Vec<(u64, Vec<u8>)>,
    fill_byte: u8,
}

impl Bus for RecordingBus {
    fn pio_read(&mut self, port: u16, data: &mut [u8]) {
        self.pio_reads.push((port, data.len()));
        data.fill(self.fill_byte);
    }
    fn pio_write(&mut self, port: u16, data: &[u8]) {
        self.pio_writes.push((port, data.to_vec()));
    }
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.mmio_reads.push((addr, data.len()));
        data.fill(self.fill_byte);
    }
    fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        self.mmio_writes.push((addr, data.to_vec()));
    }
}

/// A `TimeSource` that serves a fixed reading and records every absorbed write.
#[derive(Default)]
struct RecordingTime {
    serve_value: u64,
    rdmsr_calls: Vec<u32>,
    wrmsr_calls: Vec<(u32, u64)>,
    enforced_rdtsc_calls: u32,
    enforced_tsc_aux_calls: u32,
    enforced_rdrand_calls: u32,
    enforced_rdseed_calls: u32,
    /// The one `rip` (if any) [`TimeSource::resolve_rdseed_site`] should recognize as a known
    /// rewrite site — every other `rip` resolves to `None`, exercising the re-inject path.
    known_rdseed_site: Option<(u64, EnforcedRdseedSite)>,
}

impl TimeSource for RecordingTime {
    fn serve_rdmsr(&mut self, msr: u32) -> u64 {
        self.rdmsr_calls.push(msr);
        self.serve_value
    }
    fn absorb_wrmsr(&mut self, msr: u32, value: u64) {
        self.wrmsr_calls.push((msr, value));
    }
    fn serve_enforced_rdtsc(&mut self) -> u64 {
        self.enforced_rdtsc_calls += 1;
        self.serve_value
    }
    fn serve_enforced_tsc_aux(&mut self) -> u32 {
        self.enforced_tsc_aux_calls += 1;
        self.serve_value as u32
    }
    fn serve_enforced_rdrand(&mut self) -> u64 {
        self.enforced_rdrand_calls += 1;
        self.serve_value
    }
    fn resolve_rdseed_site(&self, rip: u64) -> Option<EnforcedRdseedSite> {
        match self.known_rdseed_site {
            Some((known_rip, site)) if known_rip == rip => Some(site),
            _ => None,
        }
    }
    fn serve_enforced_rdseed(&mut self) -> u64 {
        self.enforced_rdseed_calls += 1;
        self.serve_value
    }
}

#[test]
fn io_in_reads_from_bus_and_continues() {
    let mut bus = RecordingBus { fill_byte: 0xAB, ..Default::default() };
    let mut time = RecordingTime::default();
    let mut data = [0u8; 4];
    let outcome = dispatch_exit(Exit::IoIn(0x3F8, &mut data), &mut bus, &mut time).unwrap();
    assert_eq!(outcome, DispatchOutcome::Continue);
    assert_eq!(bus.pio_reads, vec![(0x3F8, 4)]);
    assert_eq!(data, [0xAB; 4]);
}

#[test]
fn io_out_writes_to_bus_and_continues() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime::default();
    let data = [1, 2, 3];
    let outcome = dispatch_exit(Exit::IoOut(0x60, &data), &mut bus, &mut time).unwrap();
    assert_eq!(outcome, DispatchOutcome::Continue);
    assert_eq!(bus.pio_writes, vec![(0x60, vec![1, 2, 3])]);
}

#[test]
fn mmio_read_and_write_route_to_bus() {
    let mut bus = RecordingBus { fill_byte: 0x11, ..Default::default() };
    let mut time = RecordingTime::default();
    let mut data = [0u8; 2];
    dispatch_exit(Exit::MmioRead(0x1000, &mut data), &mut bus, &mut time).unwrap();
    assert_eq!(bus.mmio_reads, vec![(0x1000, 2)]);
    assert_eq!(data, [0x11, 0x11]);

    dispatch_exit(Exit::MmioWrite(0x2000, &[9, 9]), &mut bus, &mut time).unwrap();
    assert_eq!(bus.mmio_writes, vec![(0x2000, vec![9, 9])]);
}

#[test]
fn rdmsr_is_served_from_time_source() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime { serve_value: 0xDEAD_BEEF, ..Default::default() };
    let mut out = 0u64;
    let outcome = dispatch_exit(Exit::Rdmsr(0x10, &mut out), &mut bus, &mut time).unwrap();
    assert_eq!(outcome, DispatchOutcome::Continue);
    assert_eq!(out, 0xDEAD_BEEF);
    assert_eq!(time.rdmsr_calls, vec![0x10]);
}

#[test]
fn wrmsr_is_absorbed_by_time_source() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime::default();
    dispatch_exit(Exit::Wrmsr(0x10, 42), &mut bus, &mut time).unwrap();
    assert_eq!(time.wrmsr_calls, vec![(0x10, 42)]);
}

#[test]
fn hlt_and_shutdown_report_halted() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime::default();
    assert_eq!(dispatch_exit(Exit::Hlt, &mut bus, &mut time).unwrap(), DispatchOutcome::Halted);
    assert_eq!(dispatch_exit(Exit::Shutdown, &mut bus, &mut time).unwrap(), DispatchOutcome::Halted);
}

/// Regression for a real bug (todo.md §14): `IrqWindowOpen` used to fall into `Exit::Unmodeled`,
/// so any guest that genuinely needed `boundary::PmuStepper::run_until_irq_window`'s fallback (not
/// already injectable the instant `inject_at` checked) hit the determinism-hole catch-all instead
/// of `run_until_irq_window`'s own readiness check ever running — first surfaced by a real Linux
/// kernel's early boot, which disables interrupts for real stretches.
#[test]
fn irq_window_open_continues_rather_than_faulting() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime::default();
    assert_eq!(
        dispatch_exit(Exit::IrqWindowOpen, &mut bus, &mut time).unwrap(),
        DispatchOutcome::Continue
    );
}

#[test]
fn debug_reports_single_step_boundary() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime::default();
    assert_eq!(
        dispatch_exit(Exit::Debug, &mut bus, &mut time).unwrap(),
        DispatchOutcome::SingleStepBoundary
    );
}

/// todo.md §3.3's enforced regime: a trapped `RDTSC` resolves to the work-clock's own value
/// (`serve_enforced_rdtsc`), reported back for `linux::run_one_exit` to write into EDX:EAX —
/// never resolved silently or left for a generic catch-all.
#[test]
fn rdtsc_enforced_is_served_from_time_source() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime { serve_value: 0x1234_5678_9ABC, ..Default::default() };
    let outcome = dispatch_exit(Exit::RdtscEnforced, &mut bus, &mut time).unwrap();
    assert_eq!(outcome, DispatchOutcome::ServeEnforcedRdtsc(0x1234_5678_9ABC));
    assert_eq!(time.enforced_rdtsc_calls, 1);
}

/// todo.md §14 next-actions item 2: a trapped `RDTSCP` resolves to the work-clock's own value
/// (same as `RdtscEnforced`) plus the served `IA32_TSC_AUX`, reported back for `linux::run_one_exit`
/// to write into EDX:EAX/ECX — never resolved silently or left for a generic catch-all.
#[test]
fn rdtscp_enforced_is_served_from_time_source() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime { serve_value: 0x1234_5678_9ABC, ..Default::default() };
    let outcome = dispatch_exit(Exit::RdtscpEnforced, &mut bus, &mut time).unwrap();
    assert_eq!(
        outcome,
        DispatchOutcome::ServeEnforcedRdtscp { value: 0x1234_5678_9ABC, tsc_aux: 0x5678_9ABC }
    );
    assert_eq!(time.enforced_rdtsc_calls, 1);
    assert_eq!(time.enforced_tsc_aux_calls, 1);
}

/// todo.md §3.2's enforced regime: a trapped `RDRAND` resolves to a served value tagged with its
/// guest-chosen destination GPR — reported back for `linux::run_and_convert`'s caller to write
/// there (plus RFLAGS.CF), never resolved silently or left for a generic catch-all.
#[test]
fn rdrand_enforced_is_served_from_time_source_with_its_gpr_index() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime { serve_value: 0xFEED_FACE_0102_0304, ..Default::default() };
    let outcome = dispatch_exit(Exit::RdrandEnforced { gpr_index: 6 }, &mut bus, &mut time).unwrap();
    assert_eq!(outcome, DispatchOutcome::ServeEnforcedRdrand { gpr_index: 6, value: 0xFEED_FACE_0102_0304 });
    assert_eq!(time.enforced_rdrand_calls, 1);
}

/// todo.md §4/§12 row 15: a `#UD` at a `rip` the `TimeSource` recognizes as a known `rdseed`→`UD2`
/// rewrite site is served a value tagged with that site's GPR/length — never resolved silently or
/// left for a generic catch-all.
#[test]
fn rdseed_enforced_at_a_known_site_is_served_from_time_source() {
    let mut bus = RecordingBus::default();
    let site = EnforcedRdseedSite { gpr_index: 3, length: 4 };
    let mut time = RecordingTime {
        serve_value: 0x0BAD_F00D_CAFE_BABE,
        known_rdseed_site: Some((0x1000, site)),
        ..Default::default()
    };
    let outcome = dispatch_exit(Exit::RdseedEnforced { rip: 0x1000 }, &mut bus, &mut time).unwrap();
    assert_eq!(
        outcome,
        DispatchOutcome::ServeEnforcedRdseed { rip: 0x1000, site, value: 0x0BAD_F00D_CAFE_BABE }
    );
    assert_eq!(time.enforced_rdseed_calls, 1);
}

/// The other half of todo.md §12 row 15: a `#UD` at a `rip` with no known site (a real
/// invalid-opcode fault, or a kernel `BUG()`/`WARN_ON()` that also compiles to a bare `UD2`) must
/// be re-injected, never served a guessed value — and must not even call `serve_enforced_rdseed`.
#[test]
fn rdseed_enforced_at_an_unknown_site_is_reinjected_not_served() {
    let mut bus = RecordingBus::default();
    let mut time = RecordingTime {
        known_rdseed_site: Some((0x1000, EnforcedRdseedSite { gpr_index: 3, length: 4 })),
        ..Default::default()
    };
    let outcome = dispatch_exit(Exit::RdseedEnforced { rip: 0x2000 }, &mut bus, &mut time).unwrap();
    assert_eq!(outcome, DispatchOutcome::ReinjectUd);
    assert_eq!(time.enforced_rdseed_calls, 0, "an unknown site must never draw from the entropy stream");
}

// specs/baud-vcpu.md §6 `no_unmodeled_exit_is_silent`: the run loop never leaves the dispatch
// without an `Ok`/`Err` — every unmodeled exit fails loud, and every modeled one always
// resolves. Fuzzed over a thousand random exit shapes (mirroring the spec's `random_tapes(1000)`).
proptest! {
    #[test]
    fn no_unmodeled_exit_is_silent(
        which in 0u8..14,
        port in any::<u16>(),
        addr in any::<u64>(),
        msr in any::<u32>(),
        value in any::<u64>(),
        len in 0usize..8,
        gpr_index in 0u8..16,
        rip in any::<u64>(),
        exit_name in "[a-zA-Z]{1,16}",
    ) {
        let mut bus = RecordingBus::default();
        // `rip` is always the recognized site here — the unknown-site (re-inject) path is
        // exhaustively still `Ok` either way, so this proptest only needs to prove "never a
        // panic, never silently unresolved"; the known-vs-unknown distinction itself is covered
        // by the two dedicated tests above.
        let mut time = RecordingTime {
            known_rdseed_site: Some((rip, EnforcedRdseedSite { gpr_index, length: 3 })),
            ..Default::default()
        };
        let mut buf = vec![0u8; len.max(1)];
        let mut msr_out = 0u64;
        let result = match which {
            0 => dispatch_exit(Exit::IoIn(port, &mut buf), &mut bus, &mut time),
            1 => dispatch_exit(Exit::IoOut(port, &buf), &mut bus, &mut time),
            2 => dispatch_exit(Exit::MmioRead(addr, &mut buf), &mut bus, &mut time),
            3 => dispatch_exit(Exit::MmioWrite(addr, &buf), &mut bus, &mut time),
            4 => dispatch_exit(Exit::Rdmsr(msr, &mut msr_out), &mut bus, &mut time),
            5 => dispatch_exit(Exit::Wrmsr(msr, value), &mut bus, &mut time),
            6 => dispatch_exit(Exit::Hlt, &mut bus, &mut time),
            7 => dispatch_exit(Exit::Shutdown, &mut bus, &mut time),
            8 => dispatch_exit(Exit::RdtscEnforced, &mut bus, &mut time),
            9 => dispatch_exit(Exit::RdrandEnforced { gpr_index }, &mut bus, &mut time),
            10 => dispatch_exit(Exit::RdseedEnforced { rip }, &mut bus, &mut time),
            11 => dispatch_exit(Exit::RdseedEnforced { rip: rip.wrapping_add(1) }, &mut bus, &mut time),
            12 => dispatch_exit(Exit::RdtscpEnforced, &mut bus, &mut time),
            _ => {
                let leaked: &'static str = Box::leak(exit_name.into_boxed_str());
                dispatch_exit(Exit::Unmodeled(leaked), &mut bus, &mut time)
            }
        };
        // The whole point: never a panic, and the catch-all (`which == 13`) is always `Err`,
        // every modeled exit is always `Ok`.
        if which == 13 {
            prop_assert!(result.is_err());
        } else {
            prop_assert!(result.is_ok());
        }
    }
}

#[test]
fn open_bus_reads_are_fixed_never_host_memory() {
    let mut bus = OpenBusFallback;
    let mut time = RecordingTime::default();
    let mut data = [0x00u8; 4];
    dispatch_exit(Exit::IoIn(0x9999, &mut data), &mut bus, &mut time).unwrap();
    assert_eq!(data, [OPEN_BUS_BYTE; 4]);

    let mut data = [0x00u8; 4];
    dispatch_exit(Exit::MmioRead(0xDEAD_0000, &mut data), &mut bus, &mut time).unwrap();
    assert_eq!(data, [OPEN_BUS_BYTE; 4]);

    // Writes to open bus are absorbed, never observably stored or reflected.
    dispatch_exit(Exit::IoOut(0x9999, &[1, 2, 3]), &mut bus, &mut time).unwrap();
    dispatch_exit(Exit::MmioWrite(0xDEAD_0000, &[1, 2, 3]), &mut bus, &mut time).unwrap();
}

#[test]
fn vm_creation_refuses_multiple_vcpus() {
    assert!(validate_vcpu_count(1).is_ok());
    assert!(validate_vcpu_count(0).is_err());
    assert!(validate_vcpu_count(2).is_err());
    let err = validate_vcpu_count(4).unwrap_err();
    assert_eq!(err, VmCfgError::MultipleVcpus(4));
}
