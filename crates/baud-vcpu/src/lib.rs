// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-vcpu — the single-vCPU state machine and VM-exit dispatch (specs/baud-vcpu.md).
//
// This crate is split so the exit-dispatch logic (§3) and the interrupt-injection boundary
// engine (§5) are hardware-independent, pure Rust, unit-testable on any OS — including this
// Windows dev machine, which has no `/dev/kvm`. Only `linux` (cfg-gated) touches real KVM
// ioctls; it is type-checked via `cargo check --target x86_64-unknown-linux-gnu -p baud-vcpu`
// but not yet exercised against real hardware (see todo.md §14 / CLAUDE.md).
//
// Rules (specs/baud-vcpu.md §3-4): exactly one vCPU per VM; every exit resolves to a computed
// value; the catch-all fails loud (`Err(DeterminismHole)`), never a best-effort continue;
// open-bus PIO/MMIO reads return a fixed byte, never host memory.

pub mod boundary;

#[cfg(target_os = "linux")]
pub mod linux;

use thiserror::Error;

/// A single VM exit, reduced to the fields `dispatch_exit` needs. This is baud-vcpu's own
/// vocabulary, not `kvm_ioctls::VcpuExit` directly — the `linux` module converts the real KVM
/// exit into this enum, and any KVM exit kind it does not recognize becomes `Unmodeled` rather
/// than silently vanishing. Keeping this enum crate-local (not re-exporting kvm_ioctls types)
/// is what makes `dispatch_exit` exhaustively testable without KVM (specs/baud-vcpu.md §3).
#[derive(Debug)]
pub enum Exit<'a> {
    /// An `IN` instruction on `port`; `data` must be filled before the guest resumes.
    IoIn(u16, &'a mut [u8]),
    /// An `OUT` instruction on `port` carrying `data`.
    IoOut(u16, &'a [u8]),
    /// An MMIO read at `addr`; `data` must be filled before the guest resumes.
    MmioRead(u64, &'a mut [u8]),
    /// An MMIO write at `addr` carrying `data`.
    MmioWrite(u64, &'a [u8]),
    /// `RDMSR` of `msr`; the served value must be written into `out` before resuming.
    Rdmsr(u32, &'a mut u64),
    /// `WRMSR` of `msr` carrying `value`.
    Wrmsr(u32, u64),
    /// The guest halted (`HLT`) or the VM is shutting down — both end the run cleanly.
    Hlt,
    Shutdown,
    /// A single-step / breakpoint debug exit — the boundary-walk driver (`boundary`) owns what
    /// happens next; the dispatcher just reports that a step boundary was reached.
    Debug,
    /// `RDTSC` trapped by the enforced-regime KVM module (`KVM_EXIT_BAUD_DETERMINISM`,
    /// kernel-module/baud-enforced/ENFORCEMENT_DESIGN.md) instead of executing natively — the
    /// served value must be written into EDX:EAX before the guest resumes. Unlike `Rdmsr`, the
    /// destination is a GPR pair, not a field inside the mmap'd `kvm_run`, so this variant carries
    /// no data pointer; `linux::run_one_exit` performs the `KVM_SET_REGS` write itself once
    /// `dispatch_exit` reports the value via [`DispatchOutcome::ServeEnforcedRdtsc`].
    RdtscEnforced,
    /// Any exit kind `dispatch_exit`'s caller does not recognize or does not yet model. Carries
    /// the KVM exit's name so a `DeterminismHole` names what leaked. This is the one arm that
    /// exists specifically so nothing new can silently "just continue" (specs/baud-vcpu.md §3).
    Unmodeled(&'static str),
}

/// A VM exit resolved to a value the dispatch loop's own match could not — but every exit still
/// resolved to *something* computed, never a silent continue. Guests never see this; it is the
/// canonical "determinism hole" failure the run loop fails on (specs/baud-vcpu.md §3, §7).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("determinism hole: unhandled exit `{0}` reached the run-loop catch-all")]
pub struct DeterminismHole(pub String);

/// What the dispatch loop should do after one exit was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Re-enter `KVM_RUN`; the exit was fully served.
    Continue,
    /// The guest halted or the VM is shutting down; the run loop returns `Ok(Halted)`.
    Halted,
    /// A debug/single-step exit; ownership passes to the boundary-walk driver.
    SingleStepBoundary,
    /// `Exit::RdtscEnforced` resolved to this work-clock value; `linux::run_one_exit` writes it
    /// into EDX:EAX via `KVM_SET_REGS` and re-enters `KVM_RUN` — never surfaced past that one call
    /// site (specs/baud-vcpu.md §3.3).
    ServeEnforcedRdtsc(u64),
}

/// The paravirtual bus every `IoIn`/`IoOut`/`MmioRead`/`MmioWrite` exit is routed through
/// (served by `baud-tape-device` and the console in the full VMM; specs/baud-vcpu.md §3).
pub trait Bus {
    fn pio_read(&mut self, port: u16, data: &mut [u8]);
    fn pio_write(&mut self, port: u16, data: &[u8]);
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]);
    fn mmio_write(&mut self, addr: u64, data: &[u8]);
}

/// The virtual-TSC / work-clock server every `Rdmsr`/`Wrmsr` exit is routed through
/// (`IA32_TSC`/`TSC_AUX`/`TSC_DEADLINE`, specs/baud-multiverse.md §4).
pub trait TimeSource {
    fn serve_rdmsr(&mut self, msr: u32) -> u64;
    fn absorb_wrmsr(&mut self, msr: u32, value: u64);
    /// The enforced-regime work-clock value for a trapped `RDTSC` (todo.md §3.3: "enforced = force
    /// RDTSC-exiting and return the work-clock value (bit-exact...)") — the same formula
    /// `serve_rdmsr(MSR_IA32_TSC)` already computes, since both must agree bit-for-bit.
    fn serve_enforced_rdtsc(&mut self) -> u64;
}

/// Resolve one exit deterministically (specs/baud-vcpu.md §3's match). Exhaustive over `Exit` —
/// there is no wildcard arm here; `Exit::Unmodeled` is the only path that can fail, and it always
/// does, by construction. This is what `no_unmodeled_exit_is_silent` fuzzes.
pub fn dispatch_exit(
    exit: Exit<'_>,
    bus: &mut dyn Bus,
    time: &mut dyn TimeSource,
) -> Result<DispatchOutcome, DeterminismHole> {
    match exit {
        Exit::IoIn(port, data) => {
            bus.pio_read(port, data);
            Ok(DispatchOutcome::Continue)
        }
        Exit::IoOut(port, data) => {
            bus.pio_write(port, data);
            Ok(DispatchOutcome::Continue)
        }
        Exit::MmioRead(addr, data) => {
            bus.mmio_read(addr, data);
            Ok(DispatchOutcome::Continue)
        }
        Exit::MmioWrite(addr, data) => {
            bus.mmio_write(addr, data);
            Ok(DispatchOutcome::Continue)
        }
        Exit::Rdmsr(msr, out) => {
            *out = time.serve_rdmsr(msr);
            Ok(DispatchOutcome::Continue)
        }
        Exit::Wrmsr(msr, value) => {
            time.absorb_wrmsr(msr, value);
            Ok(DispatchOutcome::Continue)
        }
        Exit::Hlt | Exit::Shutdown => Ok(DispatchOutcome::Halted),
        Exit::Debug => Ok(DispatchOutcome::SingleStepBoundary),
        Exit::RdtscEnforced => Ok(DispatchOutcome::ServeEnforcedRdtsc(time.serve_enforced_rdtsc())),
        Exit::Unmodeled(name) => Err(DeterminismHole(name.to_string())),
    }
}

/// The fixed byte an open-bus (unmapped) PIO/MMIO read must return — never host memory
/// (specs/baud-vcpu.md §3, specs/baud-multiverse.md §3's "Memory init" row).
pub const OPEN_BUS_BYTE: u8 = 0xFF;

/// A `Bus` that treats every address as open-bus: reads fill with [`OPEN_BUS_BYTE`], writes are
/// silently absorbed. `baud-multiverse` composes this as the fallback behind the real tape/
/// console device map; standing alone here it is what `open_bus_reads_are_fixed_never_host_memory`
/// exercises without needing the rest of the device bus.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenBusFallback;

impl Bus for OpenBusFallback {
    fn pio_read(&mut self, _port: u16, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE);
    }
    fn pio_write(&mut self, _port: u16, _data: &[u8]) {}
    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) {
        data.fill(OPEN_BUS_BYTE);
    }
    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) {}
}

/// A VM's vCPU configuration. `n_vcpus` is validated by [`validate_vcpu_count`] before any KVM
/// resource is created — multi-core guest determinism is out of scope (specs/baud-vcpu.md §4).
#[derive(Debug, Clone, Copy)]
pub struct VmCfg {
    pub n_vcpus: usize,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VmCfgError {
    #[error(
        "baud VMs support exactly one vCPU (requested {0}); multi-core guest determinism is out \
         of scope (specs/baud-vcpu.md §4)"
    )]
    MultipleVcpus(usize),
}

/// `Vm::create` rejects `n_vcpus != 1` (specs/baud-vcpu.md §4's table, `vm_creation_refuses_multiple_vcpus`).
pub fn validate_vcpu_count(n_vcpus: usize) -> Result<(), VmCfgError> {
    if n_vcpus == 1 {
        Ok(())
    } else {
        Err(VmCfgError::MultipleVcpus(n_vcpus))
    }
}

#[cfg(test)]
mod tests;
