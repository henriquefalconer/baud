// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The real Linux/KVM half of baud-vcpu: converting a real `kvm_ioctls::VcpuExit` into baud-vcpu's
// own `Exit` (never silently dropping an exit kind), thread affinity pinning, the
// `KVM_SET_GUEST_DEBUG` single-step toggle, and the `KVM_RUN` loop that drives `dispatch_exit`.
//
// NOTE: like `crates/baud-host/src/linux.rs`, this module is written and type-checked against the
// real `kvm-ioctls`/`kvm-bindings`/`perf-event` crate sources (`cargo check --target
// x86_64-unknown-linux-gnu -p baud-vcpu`) but has not yet been exercised on real KVM hardware —
// this dev machine has no Linux/KVM host (CLAUDE.md). `pmu` additionally documents a
// process-wide-signal simplification that a full multi-thread VMM integration must revisit.

pub mod pmu;
mod watchdog;

use crate::{
    dispatch_exit, Bus, DeterminismHole, DispatchOutcome, EnforcedRdseedSite, Exit, RunLoopError, TimeSource,
};
use kvm_bindings::{kvm_guest_debug, KVM_GUESTDBG_BLOCKIRQ, KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP};
use kvm_ioctls::{VcpuExit, VcpuFd};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use watchdog::Watchdog;

/// Not in pinned `kvm-bindings` 0.14 (invented after that crate was bindgen'd) — the out-of-tree
/// enforced-regime KVM module's own exit reason for a trapped `RDTSC`/`RDRAND`
/// (`kernel-module/baud-enforced/ENFORCEMENT_DESIGN.md`, `include/uapi/linux/kvm.h`'s
/// `KVM_EXIT_BAUD_DETERMINISM` in that patched tree). Surfaces here via `kvm-ioctls`'s existing
/// `VcpuExit::Unsupported(u32)` catch-all — no crate fork needed.
const KVM_EXIT_BAUD_DETERMINISM: u32 = 41;

/// Decode `kvm_run.hw.hardware_exit_reason`'s payload for a `KVM_EXIT_BAUD_DETERMINISM` exit —
/// the low byte names which trapped instruction this is (`rdtsc-enforce.patch`'s
/// `handle_baud_rdtsc_exit` sets `0`; `rdrand-enforce.patch`'s `handle_baud_rdrand_exit` sets `1`
/// with the destination GPR index, x86-64 ModRM numbering, packed into the next byte;
/// `rdtsc-enforce.patch`'s `handle_baud_rdtscp_exit` sets `3`, no register index — `RDTSCP`'s
/// EDX:EAX/ECX destinations are architecturally fixed, same as plain `RDTSC`'s). An unknown kind
/// becomes `Unmodeled` rather than silently guessing, same rule as every other exit.
///
/// Kind `2` (`ud2-enforce.patch`'s `handle_baud_ud2_exit`, a possible `rdseed` rewrite site) is
/// deliberately **not** decoded here — unlike RDTSC/RDTSCP/RDRAND, it carries no register/RIP info
/// in the payload (the kernel patch never skips the trapping instruction, since only userspace's
/// image-specific site table knows how far a confirmed site's `UD2`+`NOP` padding extends), so
/// `run_and_convert` special-cases it before ever calling this function, fetching RIP via its own
/// `KVM_GET_REGS`. It falls through to `Unmodeled` here only because nothing should call this
/// function with that payload directly.
fn decode_baud_determinism_exit(payload: u64) -> Exit<'static> {
    match payload & 0xFF {
        0 => Exit::RdtscEnforced,
        1 => Exit::RdrandEnforced { gpr_index: ((payload >> 8) & 0xF) as u8 },
        3 => Exit::RdtscpEnforced,
        _ => Exit::Unmodeled("BaudDeterminismUnknownKind"),
    }
}

/// Convert a real KVM exit into baud-vcpu's own vocabulary (specs/baud-vcpu.md §3). This is the
/// one place a KVM exit kind can enter the system; anything this match does not model becomes
/// `Exit::Unmodeled(name)`, which `dispatch_exit`'s catch-all always turns into
/// `Err(DeterminismHole)` — never a best-effort continue (specs/baud-multiverse.md §3's last row).
pub fn convert_exit(exit: VcpuExit<'_>) -> Exit<'_> {
    match exit {
        VcpuExit::IoIn(port, data) => Exit::IoIn(port, data),
        VcpuExit::IoOut(port, data) => Exit::IoOut(port, data),
        VcpuExit::MmioRead(addr, data) => Exit::MmioRead(addr, data),
        VcpuExit::MmioWrite(addr, data) => Exit::MmioWrite(addr, data),
        VcpuExit::X86Rdmsr(read) => {
            // baud always serves a computed value for every trapped MSR; never inject a #GP for
            // a read (specs/baud-multiverse.md §4's MSR-filter row routes IA32_TSC/TSC_AUX/
            // TSC_DEADLINE to the VMM specifically so they can always be served).
            *read.error = 0;
            Exit::Rdmsr(read.index, read.data)
        }
        VcpuExit::X86Wrmsr(write) => {
            *write.error = 0;
            Exit::Wrmsr(write.index, write.data)
        }
        VcpuExit::Hlt => Exit::Hlt,
        VcpuExit::Shutdown => Exit::Shutdown,
        VcpuExit::Debug(_) => Exit::Debug,
        VcpuExit::Unknown => Exit::Unmodeled("Unknown"),
        VcpuExit::Exception => Exit::Unmodeled("Exception"),
        VcpuExit::Hypercall(_) => Exit::Unmodeled("Hypercall"),
        VcpuExit::IrqWindowOpen => Exit::IrqWindowOpen,
        VcpuExit::FailEntry(..) => Exit::Unmodeled("FailEntry"),
        VcpuExit::Intr => Exit::Unmodeled("Intr"),
        VcpuExit::SetTpr => Exit::Unmodeled("SetTpr"),
        VcpuExit::TprAccess => Exit::Unmodeled("TprAccess"),
        VcpuExit::S390Sieic => Exit::Unmodeled("S390Sieic"),
        VcpuExit::S390Reset => Exit::Unmodeled("S390Reset"),
        VcpuExit::Dcr => Exit::Unmodeled("Dcr"),
        VcpuExit::Nmi => Exit::Unmodeled("Nmi"),
        VcpuExit::InternalError => Exit::Unmodeled("InternalError"),
        VcpuExit::Osi => Exit::Unmodeled("Osi"),
        VcpuExit::PaprHcall => Exit::Unmodeled("PaprHcall"),
        VcpuExit::S390Ucontrol => Exit::Unmodeled("S390Ucontrol"),
        VcpuExit::Watchdog => Exit::Unmodeled("Watchdog"),
        VcpuExit::S390Tsch => Exit::Unmodeled("S390Tsch"),
        VcpuExit::Epr => Exit::Unmodeled("Epr"),
        VcpuExit::SystemEvent(..) => Exit::Unmodeled("SystemEvent"),
        VcpuExit::S390Stsi => Exit::Unmodeled("S390Stsi"),
        VcpuExit::IoapicEoi(_) => Exit::Unmodeled("IoapicEoi"),
        VcpuExit::Hyperv => Exit::Unmodeled("Hyperv"),
        VcpuExit::MemoryFault { .. } => Exit::Unmodeled("MemoryFault"),
        VcpuExit::Unsupported(KVM_EXIT_BAUD_DETERMINISM) => Exit::RdtscEnforced,
        VcpuExit::Unsupported(_) => Exit::Unmodeled("Unsupported"),
    }
}

/// Pin the calling thread to exactly one physical core (specs/baud-vcpu.md §4: "Thread pinned to
/// one core"; specs/baud-host.md §5's one-core-per-VM fleet placement picks which one).
pub fn pin_thread_to_core(core_id: usize) -> io::Result<()> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core_id, &mut set);
        let rc = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Enable (`block_irq = true` also sets `KVM_GUESTDBG_BLOCKIRQ`, specs/baud-vcpu.md §5 step 3) or
/// disable single-step via `KVM_SET_GUEST_DEBUG`.
pub fn set_singlestep(vcpu: &VcpuFd, enabled: bool, block_irq: bool) -> io::Result<()> {
    let mut control = 0u32;
    if enabled {
        control |= KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_SINGLESTEP;
        if block_irq {
            control |= KVM_GUESTDBG_BLOCKIRQ;
        }
    }
    let debug = kvm_guest_debug { control, ..Default::default() };
    vcpu.set_guest_debug(&debug).map_err(io::Error::from)
}

/// Run the `KVM_RUN` loop until a `Halted` outcome, retrying transparently on a benign `EINTR` (a
/// signal arriving mid-ioctl for some unrelated reason — this call site does not care why), but
/// bounded by a real wall-clock [`Watchdog`] armed for `watchdog_budget` (`Duration::ZERO`
/// disables it — see [`Watchdog::arm`]'s doc). Without this bound a guest that makes zero VM
/// exits at all (this project's subtractive machine model has no APIC/PIT/host interrupts to
/// force one) would otherwise block this thread inside `KVM_RUN` forever (todo.md §14.1 "Still
/// open" item 1); `docs/determinism.md`'s "Known limits" §4 names the watchdog kill as the one
/// deliberately non-deterministic intervention in the whole system.
pub fn run_until_halted(
    vcpu: &mut VcpuFd,
    bus: &mut dyn Bus,
    time: &mut dyn TimeSource,
    watchdog_budget: Duration,
) -> Result<(), RunLoopError> {
    let watchdog = Watchdog::arm(watchdog_budget);
    let result = loop {
        let outcome = match run_one_exit_impl(vcpu, bus, time, Some(&watchdog.fired)) {
            Ok(outcome) => outcome,
            Err(e) => break Err(e),
        };
        match outcome {
            DispatchOutcome::Continue => continue,
            DispatchOutcome::Halted => break Ok(()),
            DispatchOutcome::SingleStepBoundary => continue, // no boundary walk in progress here
            DispatchOutcome::ServeEnforcedRdtsc(_)
            | DispatchOutcome::ServeEnforcedRdtscp { .. }
            | DispatchOutcome::ServeEnforcedRdrand { .. }
            | DispatchOutcome::ServeEnforcedRdseed { .. }
            | DispatchOutcome::ReinjectUd => {
                unreachable!("run_one_exit always resolves this to Continue before returning")
            }
        }
    };
    // Read `fired` before `disarm` consumes `watchdog` — a hole that raced the watchdog firing
    // (both true at once is possible only in the narrow window where the guest's own genuine
    // ioctl failure happens to land in the same instant the budget ran out) is reported as the
    // watchdog kill: the budget had, in fact, already elapsed either way.
    let watchdog_fired = watchdog.fired.load(Ordering::SeqCst);
    watchdog.disarm();
    result.map_err(|hole| {
        if watchdog_fired {
            RunLoopError::WatchdogKilled { budget_ms: watchdog_budget.as_millis() as u64 }
        } else {
            RunLoopError::DeterminismHole(hole)
        }
    })
}

/// What one `KVM_RUN` step converted to: either a fully-resolved [`Exit`], or a signal that the
/// *caller* must fetch `rip` itself (via its own `vcpu.get_regs()`) before it can build
/// `Exit::RdseedEnforced`. This split exists purely to satisfy the borrow checker: a function
/// whose return type is generic over `vcpu`'s own lifetime (as `Exit<'_>`'s `IoIn`/`MmioRead`
/// variants require it to be, so their `&mut [u8]` slices can borrow the mmap'd `kvm_run`) reserves
/// that *entire* borrow for its own whole execution — so *no* function that returns `Exit<'_>`
/// (or anything containing it) tied to `vcpu`'s own lifetime can also perform the extra
/// `KVM_GET_REGS` the UD2 case needs, on any path, even one that never actually borrows anything
/// through `Exit`. `run_and_convert` therefore returns this enum instead of `Exit<'_>` directly;
/// only its callers (`run_one_exit`/`pmu`'s loops), whose own functions return types with no such
/// tie, can freely reborrow `vcpu` once `run_and_convert` itself has returned.
enum ConvertedExit<'a> {
    Exit(Exit<'a>),
    /// A UD2 trapped (a possible `rdseed` rewrite site, todo.md §4) — the caller must fetch `rip`
    /// via `vcpu.get_regs()` and construct `Exit::RdseedEnforced { rip }` itself.
    RdseedTrapNeedsRip,
}

/// Run one `KVM_RUN` step and convert its result, decoding the enforced-regime
/// `KVM_EXIT_BAUD_DETERMINISM` payload (RDTSC/RDRAND/UD2, [`decode_baud_determinism_exit`]) when
/// present. `kvm_run_ptr` is captured *before* `vcpu.run()` (as a raw pointer, not a live
/// reference — the borrow it comes from ends the moment it is cast) specifically so it can be
/// dereferenced afterward without conflicting with `exit`'s own borrow of `vcpu`: `exit`'s type
/// ties a borrow of `vcpu` to the whole function (the elided lifetime in the return type), so
/// `vcpu.get_kvm_run()` cannot be called again anywhere after `vcpu.run()` while `exit` is still
/// live (including inside just one match arm — the compiler unifies the borrow across the whole
/// function, not per branch) — only a raw-pointer read sidesteps that. The UD2 case additionally
/// needs `KVM_GET_REGS`, which the same restriction rules out *inside this function* (or any
/// function returning `Exit<'_>`/[`ConvertedExit`]) entirely — see [`ConvertedExit`]'s doc; that
/// fetch happens in each of this function's own callers instead.
fn run_and_convert(vcpu: &mut VcpuFd) -> Result<ConvertedExit<'_>, kvm_ioctls::Error> {
    let kvm_run_ptr: *mut kvm_bindings::kvm_run = vcpu.get_kvm_run();
    let exit = vcpu.run()?;
    if !matches!(exit, VcpuExit::Unsupported(KVM_EXIT_BAUD_DETERMINISM)) {
        return Ok(ConvertedExit::Exit(convert_exit(exit)));
    }
    // SAFETY: `kvm_run_ptr` points at this vCPU's own mmap'd `kvm_run`, valid for the vCPU's whole
    // lifetime (well past this call). `hw` is the union member the out-of-tree kernel patch
    // (`rdrand-enforce.patch`'s `handle_baud_rdrand_exit`, `rdtsc-enforce.patch`'s
    // `handle_baud_rdtsc_exit`, `ud2-enforce.patch`'s `handle_baud_ud2_exit`) always initializes
    // whenever `exit_reason == KVM_EXIT_BAUD_DETERMINISM`, which is exactly the case just checked
    // via `exit` (itself holding no borrowed data for this variant, so this read aliases nothing
    // `exit` owns).
    let payload = unsafe { (*kvm_run_ptr).__bindgen_anon_1.hw.hardware_exit_reason };
    if payload & 0xFF == 2 {
        return Ok(ConvertedExit::RdseedTrapNeedsRip);
    }
    Ok(ConvertedExit::Exit(decode_baud_determinism_exit(payload)))
}

/// `run_and_convert`, bracketed with [`TimeSource::resume_rcb`]/[`TimeSource::pause_rcb`] so the
/// RCB-backed work-clock counter (`baud_multiverse::timesource::WorkClock`, todo.md §14
/// next-actions item 2) only accumulates branches retired during the actual `KVM_RUN` ioctl (guest
/// execution + KVM's own in-kernel vmexit handling), never the surrounding userspace dispatch code
/// — exactly the data-dependent, run-varying host code `arm_overflow`'s doc already flags as the
/// thing a `target_rcb` computation must stay free of, applied here to the value actually served
/// to the guest rather than just to injection-landing bookkeeping. Every one of this crate's real
/// `KVM_RUN` call sites goes through this wrapper instead of calling `run_and_convert` directly.
fn run_and_convert_rcb_bracketed<'a>(
    vcpu: &'a mut VcpuFd,
    time: &mut dyn TimeSource,
) -> Result<ConvertedExit<'a>, kvm_ioctls::Error> {
    time.resume_rcb();
    let result = run_and_convert(vcpu);
    time.pause_rcb();
    result
}

/// Drive exactly one `KVM_RUN` call to completion (retrying on `EINTR`) and dispatch its exit.
///
/// None of `DispatchOutcome::ServeEnforcedRdtsc`/`ServeEnforcedRdrand`/`ServeEnforcedRdseed`/
/// `ReinjectUd` ever escape this function: unlike every other exit, their destination (EDX:EAX, a
/// guest-chosen GPR for RDRAND/RDSEED, or re-injecting an exception) is not a field inside the
/// mmap'd `kvm_run` `dispatch_exit` already has a pointer into, but state reachable only via a
/// separate `KVM_GET_REGS`/`KVM_SET_REGS` (or `KVM_GET_VCPU_EVENTS`/`KVM_SET_VCPU_EVENTS`) round
/// trip — the one piece of real hardware work `dispatch_exit` itself cannot do (it has no ioctl
/// access at all, by design, so it stays testable without KVM). This is that round trip; callers
/// (`run_until_halted`, `Multiverse::step_exit`) only ever see `Continue`/`Halted`/
/// `SingleStepBoundary`.
pub fn run_one_exit(
    vcpu: &mut VcpuFd,
    bus: &mut dyn Bus,
    time: &mut dyn TimeSource,
) -> Result<DispatchOutcome, DeterminismHole> {
    // No watchdog for this entry point: `Multiverse::step_exit`'s callers (`run_until_console_len`,
    // `run_until_branch_or_halt`, interactive shell stepping) already carry their own deterministic
    // `max_exits` budget, so there is nothing here for a wall-clock kill to guard against.
    run_one_exit_impl(vcpu, bus, time, None)
}

/// [`run_one_exit`]'s real body, plus the one thing it does not need: on `EINTR`, check whether
/// `watchdog` (armed only by [`run_until_halted`]) has already fired before blindly retrying —
/// see this crate's [`RunLoopError`] and `watchdog`'s own doc for why this loop is otherwise
/// indistinguishable from a benign signal arriving for some unrelated reason.
fn run_one_exit_impl(
    vcpu: &mut VcpuFd,
    bus: &mut dyn Bus,
    time: &mut dyn TimeSource,
    watchdog: Option<&AtomicBool>,
) -> Result<DispatchOutcome, DeterminismHole> {
    loop {
        let exit = match run_and_convert_rcb_bracketed(vcpu, time) {
            Ok(ConvertedExit::Exit(exit)) => Ok(exit),
            Ok(ConvertedExit::RdseedTrapNeedsRip) => {
                // `converted` (a fieldless variant) borrows nothing further from `vcpu` past this
                // match, so this fresh `vcpu.get_regs()` is not competing with any live borrow —
                // see `ConvertedExit`'s doc for why the same fetch cannot happen one level down,
                // inside `run_and_convert` itself.
                vcpu.get_regs().map(|regs| Exit::RdseedEnforced { rip: regs.rip })
            }
            Err(e) => Err(e),
        };
        match exit {
            Ok(exit) => {
                return match dispatch_exit(exit, bus, time)? {
                    DispatchOutcome::ServeEnforcedRdtsc(value) => {
                        write_enforced_rdtsc_result(vcpu, value)
                            .map_err(|e| DeterminismHole(format!("failed to write enforced-RDTSC result: {e}")))?;
                        Ok(DispatchOutcome::Continue)
                    }
                    DispatchOutcome::ServeEnforcedRdtscp { value, tsc_aux } => {
                        write_enforced_rdtscp_result(vcpu, value, tsc_aux)
                            .map_err(|e| DeterminismHole(format!("failed to write enforced-RDTSCP result: {e}")))?;
                        Ok(DispatchOutcome::Continue)
                    }
                    DispatchOutcome::ServeEnforcedRdrand { gpr_index, value } => {
                        write_enforced_rdrand_result(vcpu, gpr_index, value)
                            .map_err(|e| DeterminismHole(format!("failed to write enforced-RDRAND result: {e}")))?;
                        Ok(DispatchOutcome::Continue)
                    }
                    DispatchOutcome::ServeEnforcedRdseed { rip, site, value } => {
                        write_enforced_rdseed_result(vcpu, rip, site, value)
                            .map_err(|e| DeterminismHole(format!("failed to write enforced-RDSEED result: {e}")))?;
                        Ok(DispatchOutcome::Continue)
                    }
                    DispatchOutcome::ReinjectUd => {
                        reinject_ud(vcpu)
                            .map_err(|e| DeterminismHole(format!("failed to re-inject #UD: {e}")))?;
                        Ok(DispatchOutcome::Continue)
                    }
                    other => Ok(other),
                };
            }
            Err(e) if e.errno() == libc::EINTR => {
                // Only `run_until_halted` ever passes `Some` here, and only once its own
                // `Watchdog` has actually fired (`watchdog::Watchdog::fired`'s doc) — any other
                // `EINTR` (this call site genuinely does not care why) keeps retrying exactly as
                // before. Returning here rather than looping is what lets a truly zero-exit
                // spinning guest ever be reclaimed at all.
                if watchdog.is_some_and(|fired| fired.load(Ordering::SeqCst)) {
                    return Err(DeterminismHole(
                        "wall-clock watchdog fired: KVM_RUN was interrupted with the guest never \
                         reaching Hlt/Shutdown"
                            .to_string(),
                    ));
                }
                continue;
            }
            Err(e) => {
                // An ioctl failure that is not a benign signal interruption is itself a
                // determinism hole: the run loop has no deterministic value to serve for "KVM
                // itself errored" (specs/baud-vcpu.md §3's catch-all covers this too).
                return Err(DeterminismHole(format!("KVM_RUN ioctl failed: {e}")));
            }
        }
    }
}

/// Real `RDTSC` (Intel SDM Vol. 2B): the high-order 32 bits of both RAX and RDX are cleared,
/// regardless of operating mode — so this overwrites the full 64-bit registers, not just their
/// low halves, matching what a guest that just executed the (now-trapped) instruction expects.
fn write_enforced_rdtsc_result(vcpu: &VcpuFd, value: u64) -> io::Result<()> {
    let mut regs = vcpu.get_regs().map_err(io::Error::from)?;
    regs.rax = value & 0xFFFF_FFFF;
    regs.rdx = value >> 32;
    vcpu.set_regs(&regs).map_err(io::Error::from)
}

/// Real `RDTSCP` (Intel SDM Vol. 2B): same EDX:EAX clearing/loading as `RDTSC` (see
/// `write_enforced_rdtsc_result`) plus ECX loaded with `IA32_TSC_AUX[31:0]` — the processor also
/// clears the high-order 32 bits of RCX, regardless of operating mode, so `tsc_aux` is
/// zero-extended into the full 64-bit register here too.
fn write_enforced_rdtscp_result(vcpu: &VcpuFd, value: u64, tsc_aux: u32) -> io::Result<()> {
    let mut regs = vcpu.get_regs().map_err(io::Error::from)?;
    regs.rax = value & 0xFFFF_FFFF;
    regs.rdx = value >> 32;
    regs.rcx = u64::from(tsc_aux);
    vcpu.set_regs(&regs).map_err(io::Error::from)
}

/// `RFLAGS.CF` (Intel SDM Vol. 1 §3.4.3.1) — the only flag bit `RDRAND` sets on success.
const X86_EFLAGS_CF: u64 = 1 << 0;
/// The five flag bits real `RDRAND` (Intel SDM Vol. 2C) always defines on completion: `CF` (set on
/// success, baud's enforced regime never reports failure), and `PF`/`AF`/`ZF`/`SF`/`OF` (always
/// cleared). Used to replace exactly these bits in RFLAGS, leaving every other bit (interrupt
/// flag, direction flag, etc.) untouched.
const RDRAND_DEFINED_FLAGS_MASK: u64 = X86_EFLAGS_CF | (1 << 2) | (1 << 4) | (1 << 6) | (1 << 7) | (1 << 11);

/// Map an x86-64 ModRM general-register index (0=RAX..7=RDI, 8=R8..15=R15 — the same numbering
/// `vmx_get_instr_info_reg` decodes from `VMX_INSTRUCTION_INFO`, `rdrand-enforce.patch`'s doc) to
/// the matching `kvm_regs` field. `rdrand-enforce.patch`'s handler always packs a 4-bit index
/// (`0..16`), so the fallback arm is defensive, not a reachable path from that patch.
fn gpr_for_index(regs: &mut kvm_bindings::kvm_regs, index: u8) -> io::Result<&mut u64> {
    Ok(match index {
        0 => &mut regs.rax,
        1 => &mut regs.rcx,
        2 => &mut regs.rdx,
        3 => &mut regs.rbx,
        4 => &mut regs.rsp,
        5 => &mut regs.rbp,
        6 => &mut regs.rsi,
        7 => &mut regs.rdi,
        8 => &mut regs.r8,
        9 => &mut regs.r9,
        10 => &mut regs.r10,
        11 => &mut regs.r11,
        12 => &mut regs.r12,
        13 => &mut regs.r13,
        14 => &mut regs.r14,
        15 => &mut regs.r15,
        _ => return Err(io::Error::other(format!("invalid RDRAND destination GPR index {index}"))),
    })
}

/// Real `RDRAND` (Intel SDM Vol. 2C) on success: the destination register (guest-chosen, decoded
/// by the kernel patch — see [`gpr_for_index`]) is loaded with the random value and `RFLAGS.CF` is
/// set with `OF`/`SF`/`ZF`/`AF`/`PF` cleared. baud's enforced regime always reports success — the
/// tape/PRNG-backed draw never "fails" the way real hardware entropy occasionally can. Like
/// `write_enforced_rdtsc_result`, this overwrites the full 64-bit register: correct for the 32-
/// and 64-bit operand sizes every guest this project targets uses; a 16-bit-operand `rdrand`
/// would leave the upper bits of its destination register untouched on real hardware, which this
/// does not model (the kernel patch does not decode operand size, only the destination register).
fn write_enforced_rdrand_result(vcpu: &VcpuFd, gpr_index: u8, value: u64) -> io::Result<()> {
    let mut regs = vcpu.get_regs().map_err(io::Error::from)?;
    *gpr_for_index(&mut regs, gpr_index)? = value;
    regs.rflags = (regs.rflags & !RDRAND_DEFINED_FLAGS_MASK) | X86_EFLAGS_CF;
    vcpu.set_regs(&regs).map_err(io::Error::from)
}

/// Real `RDSEED` (Intel SDM Vol. 2C) on success at a confirmed rewrite site: same RFLAGS/GPR
/// semantics as `write_enforced_rdrand_result` (baud's enforced regime always reports success),
/// plus the one thing RDRAND's write doesn't need — RIP must jump past the *whole* original
/// `rdseed` instruction (`site.length` bytes: 3 for `RDSEED r32`, 4 for `RDSEED r64`), not just
/// the 2-byte `UD2` the CPU actually trapped on, since `handle_baud_ud2_exit` never skips the
/// instruction itself (todo.md §4's rewrite doc: the destination GPR was recovered from
/// `baud_packages::rdseed::RdseedSite`'s pre-rewrite decode, not from anything still readable at
/// the trap site).
fn write_enforced_rdseed_result(
    vcpu: &VcpuFd,
    rip: u64,
    site: EnforcedRdseedSite,
    value: u64,
) -> io::Result<()> {
    let mut regs = vcpu.get_regs().map_err(io::Error::from)?;
    *gpr_for_index(&mut regs, site.gpr_index)? = value;
    regs.rflags = (regs.rflags & !RDRAND_DEFINED_FLAGS_MASK) | X86_EFLAGS_CF;
    regs.rip = rip.wrapping_add(u64::from(site.length));
    vcpu.set_regs(&regs).map_err(io::Error::from)
}

/// `#UD`'s vector number (Intel SDM Vol. 3A Table 6-1).
const UD_VECTOR: u8 = 6;

/// Re-inject `#UD` into the guest exactly as if baud had never intercepted it — the counterpart to
/// serving a value, for a `rip` `TimeSource::resolve_rdseed_site` does not recognize (a genuine
/// invalid-opcode bug, or a kernel `BUG()`/`WARN_ON()`, both of which also compile to a bare
/// `UD2`). RIP is left untouched (`handle_baud_ud2_exit` never advanced it), so the next
/// `KVM_RUN` delivers the exception at the same instruction a native, un-intercepted `#UD` would
/// have faulted on — real invalid-opcode handling stays completely untouched (todo.md §12 row 15).
fn reinject_ud(vcpu: &VcpuFd) -> io::Result<()> {
    let mut events = vcpu.get_vcpu_events().map_err(io::Error::from)?;
    events.exception.injected = 1;
    events.exception.nr = UD_VECTOR;
    events.exception.has_error_code = 0;
    events.exception.error_code = 0;
    vcpu.set_vcpu_events(&events).map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_bindings::kvm_regs;
    use kvm_ioctls::{MsrExitReason, ReadMsrExit, WriteMsrExit};

    // These construct `VcpuExit` values directly (plain structs/enums, no ioctl involved), so
    // `convert_exit` is exercised without any real `/dev/kvm` — only `cargo test --target
    // x86_64-unknown-linux-gnu` on an actual Linux host can run them, but they type-check via
    // `cargo check --target x86_64-unknown-linux-gnu --tests -p baud-vcpu` from this Windows box.

    #[test]
    fn modeled_exits_convert_without_becoming_unmodeled() {
        let mut buf = [0u8; 4];
        assert!(matches!(convert_exit(VcpuExit::IoIn(0x3f8, &mut buf)), Exit::IoIn(0x3f8, _)));
        assert!(matches!(convert_exit(VcpuExit::IoOut(0x3f8, &buf)), Exit::IoOut(0x3f8, _)));
        assert!(matches!(convert_exit(VcpuExit::MmioRead(0x1000, &mut buf)), Exit::MmioRead(0x1000, _)));
        assert!(matches!(convert_exit(VcpuExit::MmioWrite(0x1000, &buf)), Exit::MmioWrite(0x1000, _)));
        assert!(matches!(convert_exit(VcpuExit::Hlt), Exit::Hlt));
        assert!(matches!(convert_exit(VcpuExit::Shutdown), Exit::Shutdown));
        assert!(matches!(
            convert_exit(VcpuExit::Debug(unsafe { std::mem::zeroed() })),
            Exit::Debug
        ));
    }

    #[test]
    fn rdmsr_and_wrmsr_convert_and_clear_the_error_flag() {
        let mut error = 1u8; // start "failed" to prove convert_exit clears it
        let mut data = 0u64;
        let read = ReadMsrExit { error: &mut error, reason: MsrExitReason::Unknown, index: 0x10, data: &mut data };
        match convert_exit(VcpuExit::X86Rdmsr(read)) {
            Exit::Rdmsr(0x10, _) => {}
            other => panic!("expected Exit::Rdmsr, got {other:?}"),
        }
        assert_eq!(error, 0, "a served RDMSR must never leave the #GP error flag set");

        let mut error2 = 1u8;
        let write = WriteMsrExit { error: &mut error2, reason: MsrExitReason::Unknown, index: 0x10, data: 7 };
        match convert_exit(VcpuExit::X86Wrmsr(write)) {
            Exit::Wrmsr(0x10, 7) => {}
            other => panic!("expected Exit::Wrmsr, got {other:?}"),
        }
        assert_eq!(error2, 0);
    }

    #[test]
    fn unmodeled_exit_kinds_never_silently_pass_through() {
        assert!(matches!(convert_exit(VcpuExit::Unknown), Exit::Unmodeled("Unknown")));
        assert!(matches!(convert_exit(VcpuExit::Exception), Exit::Unmodeled("Exception")));
        assert!(matches!(convert_exit(VcpuExit::Unsupported(999)), Exit::Unmodeled("Unsupported")));
        assert!(matches!(
            convert_exit(VcpuExit::MemoryFault { flags: 0, gpa: 0, size: 0 }),
            Exit::Unmodeled("MemoryFault")
        ));
    }

    /// The enforced-regime KVM module's trapped `RDTSC` (`KVM_EXIT_BAUD_DETERMINISM = 41`)
    /// surfaces through `kvm-ioctls`' generic `Unsupported(u32)` catch-all — this is the one
    /// reason number that must convert to a modeled exit, not `Unmodeled`, unlike every other
    /// `Unsupported` value (the test above covers 999 falling through as expected).
    #[test]
    fn baud_determinism_reason_converts_to_rdtsc_enforced() {
        assert!(matches!(convert_exit(VcpuExit::Unsupported(41)), Exit::RdtscEnforced));
    }

    /// `run_and_convert` (not `convert_exit`, which never sees the payload) is what actually
    /// distinguishes RDTSC from RDRAND on a real `KVM_EXIT_BAUD_DETERMINISM` exit — this exercises
    /// the payload decode directly (`decode_baud_determinism_exit`'s bit layout,
    /// `rdrand-enforce.patch`'s doc) without needing a real vCPU.
    #[test]
    fn baud_determinism_payload_decodes_rdtsc_and_rdrand() {
        assert!(matches!(decode_baud_determinism_exit(0), Exit::RdtscEnforced));
        assert!(matches!(
            decode_baud_determinism_exit(1 | (6 << 8)),
            Exit::RdrandEnforced { gpr_index: 6 }
        ));
        assert!(matches!(
            decode_baud_determinism_exit(1 | (15 << 8)),
            Exit::RdrandEnforced { gpr_index: 15 }
        ));
        assert!(matches!(decode_baud_determinism_exit(2), Exit::Unmodeled("BaudDeterminismUnknownKind")));
        assert!(matches!(decode_baud_determinism_exit(3), Exit::RdtscpEnforced));
    }

    #[test]
    fn gp_regs_layout_matches_kvm_regs_field_order() {
        // Documents the mapping `pmu::gp_regs_from_kvm` relies on so a future field reorder in
        // kvm-bindings is caught here rather than silently scrambling ExecPoint comparisons.
        let regs = kvm_regs { rax: 1, rbx: 2, rcx: 3, rdx: 4, rsi: 5, rdi: 6, rbp: 7, rsp: 8,
            r8: 9, r9: 10, r10: 11, r11: 12, r12: 13, r13: 14, r14: 15, r15: 16, rip: 0, rflags: 0 };
        assert_eq!(pmu::gp_regs_from_kvm(&regs), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }
}
