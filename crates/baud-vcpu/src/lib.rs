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
    /// `RDRAND` trapped by the enforced-regime KVM module (same `KVM_EXIT_BAUD_DETERMINISM`
    /// mechanism as `RdtscEnforced`, distinguished by a payload byte — `linux::convert_exit`'s
    /// doc). Unlike `RdtscEnforced`'s fixed EDX:EAX, `RDRAND`'s destination register is guest-
    /// chosen and instruction-encoded, so it is carried here (0-15, x86-64 ModRM register
    /// numbering: 0=RAX..7=RDI, 8=R8..15=R15) for `linux::run_and_convert`'s caller to write the
    /// served value into; RFLAGS.CF is also set (success) as part of that write.
    RdrandEnforced { gpr_index: u8 },
    /// A `#UD` trapped by the enforced-regime KVM module's `ud2-enforce.patch` (same
    /// `KVM_EXIT_BAUD_DETERMINISM` mechanism, payload kind 2) at `rip`. Stock KVM already forces
    /// every `#UD` to exit (its own exception bitmap always includes `UD_VECTOR`, for its
    /// software-emulation fallback) — the patch only intercepts what happens next. Every `#UD` in
    /// the guest reaches here now, not just the ones `baud-packages`' build-time `rdseed`→`UD2`
    /// rewrite (todo.md §4) produced, because a bare `UD2` is also exactly what Linux's own
    /// `BUG()`/`WARN_ON()` compile to — the kernel patch cannot tell those apart by opcode alone
    /// (the original `rdseed`'s destination-register ModRM byte was overwritten with `NOP` and is
    /// gone). `dispatch_exit` must ask `TimeSource::resolve_rdseed_site` whether `rip` is a known
    /// rewrite site before serving anything.
    RdseedEnforced { rip: u64 },
    /// Any exit kind `dispatch_exit`'s caller does not recognize or does not yet model. Carries
    /// the KVM exit's name so a `DeterminismHole` names what leaked. This is the one arm that
    /// exists specifically so nothing new can silently "just continue" (specs/baud-vcpu.md §3).
    Unmodeled(&'static str),
}

/// A known `rdseed`→`UD2`+`NOP` build-time rewrite site (`baud_packages::rdseed::RdseedSite`,
/// todo.md §4): the destination GPR the original `rdseed` would have written (x86-64 ModRM
/// numbering, same as [`Exit::RdrandEnforced`]'s `gpr_index`), and the total encoded length in
/// bytes (2, 3, or 4) of the `UD2`+`NOP` sequence that replaced it — `linux::run_one_exit` needs
/// this to advance RIP exactly as far as the original `rdseed` instruction would have, not just
/// past the 2-byte `UD2` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcedRdseedSite {
    pub gpr_index: u8,
    pub length: u8,
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
    /// `Exit::RdrandEnforced`'s served value, still tagged with which GPR it goes into —
    /// `linux::run_one_exit`/`pmu` write it there via `KVM_SET_REGS` (also setting RFLAGS.CF),
    /// same "never surfaced past that one call site" rule as `ServeEnforcedRdtsc`.
    ServeEnforcedRdrand { gpr_index: u8, value: u64 },
    /// `Exit::RdseedEnforced` resolved to a known rewritten site: `value` goes into the GPR named
    /// by `site.gpr_index` (same RDRAND flag semantics as `ServeEnforcedRdrand`) and RIP advances
    /// to `rip + site.length` — past the whole `UD2`+`NOP` sequence, not just the 2-byte `UD2`.
    /// `linux::run_one_exit` performs both writes in one `KVM_SET_REGS` round trip.
    ServeEnforcedRdseed { rip: u64, site: EnforcedRdseedSite, value: u64 },
    /// `Exit::RdseedEnforced` at a `rip` with no known site: a genuine invalid-opcode fault (or a
    /// kernel `BUG()`/`WARN_ON()`), which must be re-injected into the guest exactly as if baud
    /// had never intercepted `#UD` at all (todo.md §12 row 15) — `linux::run_one_exit` does this
    /// via `KVM_SET_VCPU_EVENTS`, RIP left untouched (the trap never advanced it).
    ReinjectUd,
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
    /// The enforced-regime value for a trapped `RDRAND` (todo.md §3.2: "enforced ... serves the
    /// tape") — a deterministic draw that must reproduce identically across a double-run of the
    /// same tape, but is otherwise independent of `serve_enforced_rdtsc`'s work-clock formula.
    fn serve_enforced_rdrand(&mut self) -> u64;
    /// Look up whether `rip` is a known `rdseed`→`UD2` build-time rewrite site (todo.md §4) —
    /// `None` for a genuine invalid-opcode fault (including a real kernel `BUG()`/`WARN_ON()`,
    /// which also compiles to a bare `UD2`), which `dispatch_exit` must re-inject rather than
    /// serve a value for.
    fn resolve_rdseed_site(&self, rip: u64) -> Option<EnforcedRdseedSite>;
    /// The enforced-regime value for a trapped, confirmed `rdseed` site (todo.md §3.8: "the value
    /// comes from the same tape-seeded entropy sub-stream as rdrand").
    fn serve_enforced_rdseed(&mut self) -> u64;
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
        Exit::RdrandEnforced { gpr_index } => {
            Ok(DispatchOutcome::ServeEnforcedRdrand { gpr_index, value: time.serve_enforced_rdrand() })
        }
        Exit::RdseedEnforced { rip } => match time.resolve_rdseed_site(rip) {
            Some(site) => {
                Ok(DispatchOutcome::ServeEnforcedRdseed { rip, site, value: time.serve_enforced_rdseed() })
            }
            None => Ok(DispatchOutcome::ReinjectUd),
        },
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
