// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The capability-check seam: `CapabilityChecks` is what `Host::probe()` reads from a real host
// (via the `linux` / `unsupported` modules) and what tests inject to synthesize hosts that would
// otherwise require real KVM hardware to exercise (specs/baud-host.md §6).

use crate::{Probe, Regime, Topology, Vendor};

/// One host's raw, independently-observed capabilities. Each method is a single check — no
/// method may infer its answer from another (specs/baud-host.md §3's table, one row each).
pub trait CapabilityChecks {
    /// `/dev/kvm` exists and opens.
    fn kvm_present(&self) -> bool;
    /// The virtualization-extensions CPU flag is present (`vmx` Intel / `svm` AMD).
    fn vmx_present(&self) -> bool;
    fn vendor(&self) -> Vendor;
    /// `KVM_SET_CPUID2` round-trips a masked leaf.
    fn cpuid_control_ok(&self) -> bool;
    /// The host TSC is stable (`KVM_GET_TSC_KHZ` did not return `-EIO`).
    fn tsc_stable(&self) -> bool;
    /// `KVM_X86_SET_MSR_FILTER` accepted.
    fn msr_filter_ok(&self) -> bool;
    /// `KVM_SET_GUEST_DEBUG` single-step accepted.
    fn singlestep_ok(&self) -> bool;
    /// A fixed userspace loop's retired-conditional-branch count is identical across two runs.
    fn rcb_deterministic(&self) -> bool;
    /// Nested-virtualization support (`kvm_intel nested=1` or vendor equivalent).
    fn nested_virt(&self) -> bool;
    /// The out-of-tree enforced-regime KVM module is loaded.
    fn enforced_module_present(&self) -> bool;
    /// The core topology used for fleet placement (specs/baud-host.md §5).
    fn topology(&self) -> Topology;
}

/// Apply the regime decision (specs/baud-host.md §4) to a set of checks.
///
/// Required for *any* regime: `/dev/kvm`, the vmx/svm flag, a stable TSC, and a deterministic
/// branch counter — without these the host cannot run baud at all (`Regime::Rejected`).
/// Required for `Cooperative` on top of that: CPUID control, MSR filtering, and single-step —
/// the mechanisms the cooperative regime itself is built from (§7 of specs/baud-multiverse.md).
/// `Enforced` additionally needs an Intel host and the out-of-tree module.
pub fn compute_probe(checks: &dyn CapabilityChecks) -> Probe {
    let kvm = checks.kvm_present();
    let vmx = checks.vmx_present();
    let vendor = checks.vendor();
    let cpuid = checks.cpuid_control_ok();
    let tsc_stable = checks.tsc_stable();
    let msr_filter = checks.msr_filter_ok();
    let singlestep = checks.singlestep_ok();
    let rcb_deterministic = checks.rcb_deterministic();
    let nested = checks.nested_virt();

    let base = |regime, reason: Option<String>| Probe {
        kvm, vmx, cpuid, tsc_stable, msr_filter, singlestep, rcb_deterministic, nested, vendor,
        regime, reason,
    };

    // Capabilities without which no regime — not even cooperative — can run at all.
    let hard_gate: [(&str, bool, &str); 4] = [
        ("/dev/kvm", kvm, "expose /dev/kvm to this host (bare-metal, or a nested-virt host with kvm_intel nested=1)"),
        ("vmx/svm CPU flag", vmx, "enable virtualization extensions (VT-x/AMD-V) in firmware/BIOS"),
        ("stable TSC", tsc_stable, "pin constant-/invariant-TSC (disable C-states/turbo); KVM_GET_TSC_KHZ must not return -EIO"),
        ("branch-counter determinism", rcb_deterministic, "this microarchitecture's retired-conditional-branch counter is not reproducible across two runs of the same loop"),
    ];
    if let Some((name, _, remediation)) = hard_gate.iter().find(|(_, ok, _)| !ok) {
        return base(Regime::Rejected, Some(format!("{name} unavailable: {remediation}")));
    }

    // Capabilities the cooperative regime itself needs to serve determinism.
    let cooperative_gate: [(&str, bool, &str); 3] = [
        ("CPUID control", cpuid, "KVM_SET_CPUID2 did not round-trip a masked leaf"),
        ("MSR filter", msr_filter, "KVM_X86_SET_MSR_FILTER was not accepted"),
        ("single-step", singlestep, "KVM_SET_GUEST_DEBUG single-step was not accepted"),
    ];
    if let Some((name, _, remediation)) = cooperative_gate.iter().find(|(_, ok, _)| !ok) {
        return base(Regime::Rejected, Some(format!("{name} unavailable: {remediation}")));
    }

    match vendor {
        Vendor::Intel if checks.enforced_module_present() => base(Regime::Enforced, None),
        Vendor::Amd => base(
            Regime::Cooperative,
            Some("AMD host: enforced regime unverified (phase-2, specs/baud-host.md §8); cooperative available".into()),
        ),
        _ => base(Regime::Cooperative, None),
    }
}

/// Test-only fakes: build [`CapabilityChecks`] implementations without touching real hardware,
/// so §6's tests (`capacity_refuses_sibling_split`, `doctor_checks_kvm`,
/// `rejected_host_names_the_failing_check`) run on any dev machine, KVM or not.
#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::{CoreTopology, Topology};

    #[derive(Debug, Clone)]
    pub struct FakeChecks {
        pub kvm: bool,
        pub vmx: bool,
        pub vendor: Vendor,
        pub cpuid: bool,
        pub tsc_stable: bool,
        pub msr_filter_ok: bool,
        pub singlestep_ok: bool,
        pub rcb_deterministic: bool,
        pub nested: bool,
        pub enforced_module_present: bool,
        pub topology: Topology,
    }

    impl CapabilityChecks for FakeChecks {
        fn kvm_present(&self) -> bool { self.kvm }
        fn vmx_present(&self) -> bool { self.vmx }
        fn vendor(&self) -> Vendor { self.vendor }
        fn cpuid_control_ok(&self) -> bool { self.cpuid }
        fn tsc_stable(&self) -> bool { self.tsc_stable }
        fn msr_filter_ok(&self) -> bool { self.msr_filter_ok }
        fn singlestep_ok(&self) -> bool { self.singlestep_ok }
        fn rcb_deterministic(&self) -> bool { self.rcb_deterministic }
        fn nested_virt(&self) -> bool { self.nested }
        fn enforced_module_present(&self) -> bool { self.enforced_module_present }
        fn topology(&self) -> Topology { self.topology.clone() }
    }

    /// A fully-capable Intel host with the enforced module present.
    pub fn fake_checks_ok(topology: Topology) -> FakeChecks {
        FakeChecks {
            kvm: true, vmx: true, vendor: Vendor::Intel, cpuid: true, tsc_stable: true,
            msr_filter_ok: true, singlestep_ok: true, rcb_deterministic: true, nested: true,
            enforced_module_present: true, topology,
        }
    }

    /// A host with everything else fine, but no `/dev/kvm`.
    pub fn no_kvm() -> FakeChecks {
        let mut c = fake_checks_ok(single_core_topology(1));
        c.kvm = false;
        c
    }

    /// `n` physical cores, each with two SMT sibling logical CPUs (hyperthreaded).
    pub fn hyperthreaded_topology(n: usize) -> Topology {
        let cores = (0..n)
            .map(|i| CoreTopology { physical_id: i, sibling_threads: vec![2 * i, 2 * i + 1] })
            .collect();
        Topology { cores, housekeeping_reserved: 0 }
    }

    /// `n` physical cores, one logical CPU each (SMT disabled).
    pub fn single_core_topology(n: usize) -> Topology {
        let cores = (0..n)
            .map(|i| CoreTopology { physical_id: i, sibling_threads: vec![i] })
            .collect();
        Topology { cores, housekeeping_reserved: 0 }
    }
}
