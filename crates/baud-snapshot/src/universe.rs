// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The enumerated capture set (specs/baud-snapshot.md §3: "omitting any field diverges the
// restore") and the ordered restore plan (§6). Hardware-independent: every field here is an
// opaque byte blob or a small value type, filled in by `linux::capture` from the real
// `KVM_GET_*` ioctls and consumed in order by `linux::restore` — this module owns *what* must be
// captured and *in what order* it must come back, not the ioctls themselves, so both guarantees
// are unit-tested here without any KVM/perf hardware (same split as `baud-vcpu::boundary` /
// `baud-multiverse::timesource`).

use serde::{Deserialize, Serialize};

use crate::msr::{MSR_IA32_TSC, MSR_IA32_TSC_DEADLINE};
use crate::page_store::PageRef;

/// One MSR value pair, as `KVM_GET_MSRS`/`KVM_SET_MSRS` exchange them (`kvm_msr_entry { index,
/// data }`, reserved/pad fields omitted since KVM never reads them back). A portable mirror of
/// `kvm_bindings::kvm_msr_entry` — same rationale as `cpuid::CpuidEntry`'s portable `CpuidLeaf` in
/// `baud-multiverse`: lets the ordering logic below be tested on this Windows dev machine with no
/// `kvm-bindings` linux-only type involved at all. `Serialize`/`Deserialize` back `wire.rs`'s
/// `UniverseBody` (every field here is already a plain value type, nothing to project away).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsrWrite {
    pub index: u32,
    pub data: u64,
}

/// Reorder `msrs` in place so [`MSR_IA32_TSC`] precedes [`MSR_IA32_TSC_DEADLINE`] (specs/
/// baud-snapshot.md §6: "Restore `IA32_TSC` before `IA32_TSC_DEADLINE`" — `KVM_SET_MSRS` applies
/// every entry in one batch ioctl, in array order, and KVM's own TSC-deadline-timer arming logic
/// reads the current TSC value at the moment the deadline MSR is written; writing the deadline
/// before the TSC base is rebased would arm it against the *old* TSC). Every other MSR keeps its
/// relative order (a stable sort by a 3-way rank: TSC=0, everything else=1, TSC_DEADLINE=2), so
/// this never reorders MSRs the spec has no ordering opinion about.
pub fn order_msrs_tsc_first(msrs: &mut [MsrWrite]) {
    msrs.sort_by_key(|m| match m.index {
        MSR_IA32_TSC => 0u8,
        MSR_IA32_TSC_DEADLINE => 2,
        _ => 1,
    });
}

/// The capture set's vCPU-state row (specs/baud-snapshot.md §3): every `KVM_GET_*` call whose
/// omission would diverge the restored universe's future execution. Each field is the exact bytes
/// `KVM_GET_*` returned (or, for `msrs`, the parsed entry list, already ordered by
/// [`order_msrs_tsc_first`]) — this crate does not interpret the bytes, only sequences their
/// restore.
///
/// **Deliberately excludes `KVM_GET_LAPIC`** (a real gap the spec's original capture-set list
/// assumed away, found and fixed against real KVM hardware): `KVM_GET_LAPIC`/`KVM_SET_LAPIC` only
/// succeed once `KVM_CREATE_IRQCHIP` has registered an in-kernel local APIC, but this workspace's
/// VMM (`baud_multiverse::linux::create_vm_vcpu_shell`) never creates one — H4's arm-early-then-
/// single-step engine (specs/baud-vcpu.md §5) injects interrupts directly via `KVM_INTERRUPT`,
/// bypassing LAPIC emulation entirely, so there is no in-kernel APIC state to capture in the first
/// place (`KVM_GET_LAPIC` fails with `EINVAL` on this VMM's vCPUs). Any interrupt state that
/// direct-injection needs to preserve across a restore (e.g. a still-pending vector) is already
/// covered by `events` (`KVM_GET_VCPU_EVENTS`, which mirrors `KVM_INTERRUPT`'s own injected-
/// interrupt bookkeeping) — omitting LAPIC is not a missing field, it names a field this VMM's
/// architecture has no analogue for. `Serialize`/`Deserialize` back `wire.rs`'s `UniverseBody`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuState {
    /// `KVM_GET_REGS` (`kvm_regs`, raw bytes).
    pub regs: Vec<u8>,
    /// `KVM_GET_SREGS` (`kvm_sregs`, raw bytes).
    pub sregs: Vec<u8>,
    /// `KVM_GET_MSRS`, already TSC-first ordered.
    pub msrs: Vec<MsrWrite>,
    /// `KVM_GET_XSAVE2` (FPU/SSE/AVX/AMX extended state, raw bytes).
    pub xsave: Vec<u8>,
    /// `KVM_GET_XCRS` (extended control registers, raw bytes).
    pub xcrs: Vec<u8>,
    /// `KVM_GET_VCPU_EVENTS` (pending interrupts/exceptions/NMIs, raw bytes).
    pub events: Vec<u8>,
    /// `KVM_GET_MP_STATE` (raw bytes — small enum, kept as bytes for the same "this crate doesn't
    /// interpret it" reason as everything else here).
    pub mp_state: Vec<u8>,
}

/// The capture set's clock row (specs/baud-snapshot.md §3): `KVM_GET_CLOCK` + `KVM_GET_TSC_KHZ`
/// plus the work-clock anchor (todo.md §5: "capture the work-clock anchor" — restoring a timer
/// guest without this would resume its virtual TSC from the wrong base, per
/// `baud-multiverse::timesource::WorkClock`'s `base + k * rcb` formula). `Serialize`/
/// `Deserialize` back `wire.rs`'s `UniverseBody`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockState {
    /// `KVM_GET_CLOCK` (`kvm_clock_data`, raw bytes).
    pub kvm_clock: Vec<u8>,
    /// `KVM_GET_TSC_KHZ` — restored *before* the vCPU that will use it is given any other state
    /// (see [`restore_plan`]'s first step).
    pub tsc_khz: u32,
    /// `WorkClock`'s `base` at the moment of capture, so a restored `WorkClock` resumes reading
    /// exactly the same virtual-TSC sequence a straight run would have produced from this point.
    pub work_clock_base: u64,
    /// `WorkClock::current_rcb()` at the moment of capture (found missing by H5's real-hardware
    /// `snapshot_roundtrip_is_bit_identical` test): a restored guest's branch counter is a *new*
    /// `perf_event` fd starting from zero, not a continuation of the original's hardware count, so
    /// without this anchor a restored `WorkClock` would report an RCB value discontinuous with the
    /// guest's true cumulative branch count (`WorkClock::rcb_offset`'s doc has the full mechanism).
    pub rcb_anchor: u64,
    /// The last value the guest wrote to `IA32_TSC_DEADLINE`, as the software work-clock currently
    /// serves it (`baud_multiverse::timesource::WorkClock::tsc_deadline`) — captured separately
    /// from `vcpu.msrs` because once the MSR filter routes this MSR to userspace
    /// (`baud_multiverse::linux::configure_msr_filter`), `KVM_GET_MSRS` never sees the guest's real
    /// write: KVM's own copy of a filtered MSR is never updated by an intercepted `wrmsr`, so
    /// without this field a restore would silently resume with a deadline of whatever KVM's
    /// internal default is, not what the guest actually armed.
    pub tsc_deadline: u64,
    /// The last value the guest wrote to `IA32_TSC_AUX`, same rationale as `tsc_deadline` above.
    pub tsc_aux: u64,
    /// `WorkClock::entropy_state()` at the moment of capture — the enforced-regime `RDRAND`
    /// entropy stream's internal PRNG state, software-only exactly like `tsc_deadline`/`tsc_aux`
    /// above (no KVM ioctl knows about it). Without this, a restored guest's next `rdrand` would
    /// repeat the seed's already-served values instead of continuing the sequence a straight run
    /// would have produced (todo.md §3.2, `baud_multiverse::timesource::WorkClock::restore`'s doc).
    pub entropy_state: u64,
}

/// The capture set's device row (specs/baud-snapshot.md §3): the tape-device cursor (how many
/// tape bytes the guest has consumed) and an opaque serialized console state. This crate does not
/// know how to serialize `baud-multiverse`'s `Console`/`TapeBus` types (that would make this a
/// dependency of the crate that is meant to depend on *it*, specs/baud-snapshot.md §2's diagram)
/// — callers hand in whatever bytes their own device model produces and are responsible for
/// deserializing them back on restore. `Serialize`/`Deserialize` back `wire.rs`'s `UniverseBody`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    pub tape_cursor: u64,
    pub console: Vec<u8>,
}

/// A complete captured VM state (specs/baud-snapshot.md §1's "universe": "guest RAM + full vCPU
/// state + VM clock/TSC + tape-device cursor + console state"). `ram` is indexed by page number
/// (page `i` covers guest-physical `[i * PAGE_SIZE, (i+1) * PAGE_SIZE)`); pages are
/// [`PageRef`]s from a shared [`crate::PageStore`] so identical content across universes is one
/// allocation, not a copy (§4).
#[derive(Clone, Debug)]
pub struct Universe {
    pub ram: Vec<PageRef>,
    pub vcpu: VcpuState,
    pub clock: ClockState,
    pub device: DeviceState,
    /// CPUID leaf-1 EAX (family/model/stepping — the "processor signature") of the host that
    /// captured this universe. [`model_matches`] compares this against the restoring host's own
    /// signature (specs/baud-snapshot.md §6 point 4, §8's "restore on wrong hardware diverges
    /// silently" threat).
    pub cpu_signature: u32,
}

/// One ordered step of a restore (specs/baud-snapshot.md §6). [`restore_plan`] is the single
/// source of truth for the sequence; `linux::restore` executes it by matching on each variant in
/// order and issuing the corresponding real ioctl — the *order* is what is unit-tested here
/// (`restore_order_matches_the_spec`), independent of whether the ioctls themselves can run on
/// this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreStep {
    /// `KVM_SET_TSC_KHZ` — first, because every other vCPU-state write (`SetVcpuMsrs` especially,
    /// which restores `IA32_TSC` itself) must land against the frequency the guest will actually
    /// run at, not whatever default the vCPU was created with.
    SetTscKhz,
    /// Register the captured RAM pages as the guest's memory-backing.
    RegisterRam,
    SetVcpuRegs,
    SetVcpuSregs,
    /// Already TSC-before-TSC_DEADLINE ordered within the entry list itself
    /// ([`order_msrs_tsc_first`]) — this step's *position* in the plan (after regs/sregs, before
    /// lapic/xsave/etc.) has no ordering requirement of its own beyond "before `SetVmClock`".
    SetVcpuMsrs,
    SetVcpuXsave,
    SetVcpuXcrs,
    SetVcpuEvents,
    SetVcpuMpState,
    /// `KVM_SET_CLOCK` — after every vCPU-state field, so the VM-wide clock is the last
    /// TSC-adjacent thing touched (mirrors capture order: clock is read last in `linux::capture`
    /// too, for the same "don't let anything after it perturb what it reports" reason).
    SetVmClock,
    /// Restore the tape-device cursor and console — device state, deliberately last: nothing
    /// above depends on it, and a caller re-wiring the console into a live shell
    /// (specs/baud-snapshot.md §5's `shell_into_universe_resumes`) wants every other piece of the
    /// universe already in place first.
    RestoreDevice,
}

/// The fixed restore sequence for any [`Universe`] (specs/baud-snapshot.md §6, points 1-3).
/// A pure function of nothing but the step kinds themselves — every `Universe` restores through
/// the exact same ordered plan, so there is no per-universe variation for a caller to get wrong.
pub fn restore_plan() -> [RestoreStep; 11] {
    use RestoreStep::*;
    [
        SetTscKhz,
        RegisterRam,
        SetVcpuRegs,
        SetVcpuSregs,
        SetVcpuMsrs,
        SetVcpuXsave,
        SetVcpuXcrs,
        SetVcpuEvents,
        SetVcpuMpState,
        SetVmClock,
        RestoreDevice,
    ]
}

/// specs/baud-snapshot.md §6 point 4 / §8's "restore on wrong hardware diverges silently" threat:
/// a restore may proceed only if the CPU signatures match, or if a CPUID template is normalizing
/// leaves across models (`template_active`). Pure comparison — the actual CPUID-leaf reads on
/// both sides are `linux::capture`'s and the restoring host's job.
pub fn model_matches(captured_signature: u32, current_signature: u32, template_active: bool) -> bool {
    template_active || captured_signature == current_signature
}

/// Indices of pages in `current` whose content differs from the corresponding page in `base`
/// (specs/baud-snapshot.md §5: "rewind copies back only dirtied pages ... cost ∝ change, not
/// machine size" — the pure planning half of `KVM_CAP_DIRTY_LOG_RING`; the real dirty-ring
/// bookkeeping is `linux`'s job once that lands, but the *guarantee* — reset touches only what
/// actually changed — is provable here independent of it). `is_same_allocation` is checked first
/// as a cheap pre-filter (two `PageRef`s a branch never wrote to are the literal same `Arc`,
/// specs/baud-snapshot.md §4) before falling back to `PageRef`'s content-hash equality, so this
/// never does a full byte-for-byte compare on a page that was never even touched.
pub fn dirty_pages(base: &[PageRef], current: &[PageRef]) -> Vec<usize> {
    base.iter()
        .zip(current.iter())
        .enumerate()
        .filter(|(_, (a, b))| !a.is_same_allocation(b) && *a != *b)
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_store::PageStore;

    #[test]
    fn order_msrs_tsc_first_places_tsc_before_tsc_deadline_regardless_of_input_order() {
        let mut msrs = vec![
            MsrWrite { index: 0x1234, data: 1 },
            MsrWrite { index: MSR_IA32_TSC_DEADLINE, data: 2 },
            MsrWrite { index: 0x5678, data: 3 },
            MsrWrite { index: MSR_IA32_TSC, data: 4 },
        ];
        order_msrs_tsc_first(&mut msrs);
        let tsc_pos = msrs.iter().position(|m| m.index == MSR_IA32_TSC).unwrap();
        let deadline_pos = msrs.iter().position(|m| m.index == MSR_IA32_TSC_DEADLINE).unwrap();
        assert!(tsc_pos < deadline_pos, "IA32_TSC must precede IA32_TSC_DEADLINE after ordering");
    }

    #[test]
    fn order_msrs_tsc_first_preserves_relative_order_of_unrelated_msrs() {
        let mut msrs = vec![
            MsrWrite { index: 0xAAAA, data: 1 },
            MsrWrite { index: 0xBBBB, data: 2 },
            MsrWrite { index: 0xCCCC, data: 3 },
        ];
        let before = msrs.clone();
        order_msrs_tsc_first(&mut msrs);
        assert_eq!(msrs, before, "no TSC/TSC_DEADLINE entries present -> stable sort must not reorder");
    }

    #[test]
    fn order_msrs_tsc_first_is_already_ordered_input_stays_ordered() {
        let mut msrs = vec![
            MsrWrite { index: MSR_IA32_TSC, data: 1 },
            MsrWrite { index: 0x9999, data: 2 },
            MsrWrite { index: MSR_IA32_TSC_DEADLINE, data: 3 },
        ];
        order_msrs_tsc_first(&mut msrs);
        assert_eq!(msrs[0].index, MSR_IA32_TSC);
        assert_eq!(msrs[2].index, MSR_IA32_TSC_DEADLINE);
    }

    /// specs/baud-snapshot.md §6 points 1-3: TSC frequency before any vCPU-state write, RAM
    /// registered before vCPU registers, and the VM clock restored only after every vCPU-state
    /// field — this is the direct test of that ordering guarantee.
    #[test]
    fn restore_order_matches_the_spec() {
        let plan = restore_plan();
        assert_eq!(plan[0], RestoreStep::SetTscKhz, "TSC frequency must be set before anything else");
        let ram_pos = plan.iter().position(|s| *s == RestoreStep::RegisterRam).unwrap();
        let first_vcpu_state_pos = plan
            .iter()
            .position(|s| matches!(s, RestoreStep::SetVcpuRegs))
            .unwrap();
        assert!(ram_pos < first_vcpu_state_pos, "RAM must be registered before vCPU registers");

        let clock_pos = plan.iter().position(|s| *s == RestoreStep::SetVmClock).unwrap();
        let last_vcpu_state_pos = plan
            .iter()
            .rposition(|s| {
                matches!(
                    s,
                    RestoreStep::SetVcpuRegs
                        | RestoreStep::SetVcpuSregs
                        | RestoreStep::SetVcpuMsrs
                        | RestoreStep::SetVcpuXsave
                        | RestoreStep::SetVcpuXcrs
                        | RestoreStep::SetVcpuEvents
                        | RestoreStep::SetVcpuMpState
                )
            })
            .unwrap();
        assert!(clock_pos > last_vcpu_state_pos, "VM clock restores after every vCPU-state field");

        assert_eq!(plan.last(), Some(&RestoreStep::RestoreDevice), "device state restores last");
    }

    #[test]
    fn model_matches_requires_equal_signature_unless_template_active() {
        assert!(model_matches(0x1234, 0x1234, false));
        assert!(!model_matches(0x1234, 0x5678, false), "mismatched signature without a template must refuse");
        assert!(model_matches(0x1234, 0x5678, true), "an active CPUID template normalizes the mismatch");
    }

    fn page(fill: u8) -> [u8; crate::page_store::PAGE_SIZE] {
        [fill; crate::page_store::PAGE_SIZE]
    }

    /// specs/baud-snapshot.md §5's "reset cost scales with write-set, not total RAM": mutate a
    /// known subset of pages and assert the dirty set's size equals exactly that subset, not the
    /// total page count.
    #[test]
    fn dirty_pages_reports_exactly_the_changed_pages_not_total_ram() {
        let mut store = PageStore::new();
        const TOTAL: usize = 500;
        const CHANGED: usize = 7;

        let base: Vec<_> = (0..TOTAL).map(|_| store.intern(&page(0))).collect();
        let mut current = base.clone();
        for (i, slot) in current.iter_mut().enumerate().take(CHANGED) {
            *slot = store.intern(&page((i + 1) as u8));
        }

        let dirty = dirty_pages(&base, &current);
        assert_eq!(dirty.len(), CHANGED, "dirty count must equal the actual write set, not TOTAL={TOTAL}");
        assert_eq!(dirty, (0..CHANGED).collect::<Vec<_>>());
    }

    #[test]
    fn unwritten_pages_short_circuit_on_shared_allocation_without_a_byte_compare() {
        let mut store = PageStore::new();
        let base: Vec<_> = (0..10).map(|_| store.intern(&page(0))).collect();
        let current = base.clone(); // every page re-interned identically -> same Arc, no writes
        assert!(dirty_pages(&base, &current).is_empty());
    }
}
