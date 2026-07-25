// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-host — the KVM-capable host manager (specs/baud-host.md)
//
// Probes the capabilities baud-multiverse needs, decides which determinism regime the host
// supports (§4), and places a fleet of single-vCPU VMs across physical cores, never splitting
// hyperthread siblings (§5).
//
// Rules:
// - No run starts on a host missing a required capability; `probe()` reports every check.
// - A failing capability downgrades the regime and names itself in `reason` — never hidden.
// - Deps = {kvm-ioctls, kvm-bindings, libc (Linux only), serde}. Soft budget <= 1,500 LOC.

mod checks;
mod placement;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

pub use checks::CapabilityChecks;
pub use placement::{CoreTopology, Placement, PlacementError, Topology};

use serde::{Deserialize, Serialize};

/// The CPU vendor of the host, as reported by CPUID leaf `0H`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Intel,
    Amd,
    Other,
}

/// The result of probing one host's capabilities (specs/baud-host.md §3).
///
/// Every field is a real, independently-observed check — never inferred from another field or
/// assumed true. A capability that could not be verified is `false`, and if it was required for
/// the regime actually granted, `reason` says which one and how to fix it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    /// `/dev/kvm` present and openable, and the host CPU reports the virtualization flag
    /// (`vmx` on Intel, `svm` on AMD).
    pub kvm: bool,
    /// The virtualization-extensions CPU flag (`vmx`/Intel or `svm`/AMD) is present.
    pub vmx: bool,
    /// `KVM_SET_CPUID2` round-trips a masked leaf (the CPUID-control path §3.2 depends on).
    pub cpuid: bool,
    /// The host TSC is stable (`KVM_GET_TSC_KHZ` does not fail with `-EIO`).
    pub tsc_stable: bool,
    /// `KVM_X86_SET_MSR_FILTER` is accepted.
    pub msr_filter: bool,
    /// `KVM_SET_GUEST_DEBUG` with `KVM_GUESTDBG_SINGLESTEP` is accepted.
    pub singlestep: bool,
    /// The retired-conditional-branch counter is deterministic on this exact silicon: a fixed
    /// userspace loop run twice yields an identical branch count.
    pub rcb_deterministic: bool,
    /// Nested-virtualization support is present (only meaningful when baud itself runs inside a
    /// VM; `false` on bare metal is not a failure).
    pub nested: bool,
    pub vendor: Vendor,
    /// The out-of-tree enforced-regime KVM module (`kernel-module/baud-enforced/`) is loaded
    /// right now, so RDTSC/RDRAND/`#UD` are hardware-trapped even for an adversarial guest.
    pub enforced_module_present: bool,
    /// Set whenever this host is not the best case (a required capability failed, or the vendor
    /// is AMD, whose enforced-regime intercepts are unverified) — names the failing check and its
    /// remediation. `None` only when the host is the best it can offer.
    pub reason: Option<String>,
}

impl Probe {
    /// Every capability baud needs to run at all is present — a guest that routes
    /// entropy/clock/input through the tape device gets full determinism here, no hardware
    /// module needed (specs/baud-host.md §4's "cooperative" case).
    pub fn is_runnable(&self) -> bool {
        self.kvm
            && self.vmx
            && self.tsc_stable
            && self.rcb_deterministic
            && self.cpuid
            && self.msr_filter
            && self.singlestep
    }

    /// This host additionally hardware-traps RDTSC/RDRAND/`#UD` via the enforced-regime KVM
    /// module, so even an adversarial guest that ignores the tape device is reproducible
    /// (specs/baud-host.md §4's "enforced" case).
    pub fn is_enforced_capable(&self) -> bool {
        self.is_runnable() && self.vendor == Vendor::Intel && self.enforced_module_present
    }
}

/// A probed host: the capability [`Probe`] plus the core topology used for fleet placement.
///
/// `Host` derefs to its [`Probe`] so `host.kvm`, `host.regime`, etc. read directly, while
/// `host.capacity()` / `host.place(n)` use the topology gathered alongside the probe.
#[derive(Debug, Clone)]
pub struct Host {
    report: Probe,
    topology: Topology,
}

impl std::ops::Deref for Host {
    type Target = Probe;
    fn deref(&self) -> &Probe {
        &self.report
    }
}

impl Host {
    /// Probe the real, running host: opens `/dev/kvm`, reads `/proc/cpuinfo` and `/sys` topology,
    /// and (on Linux) exercises the KVM ioctls in §3's table. On a non-Linux host — including this
    /// dev machine unless it is a Linux/WSL2 box with KVM wired through — every KVM-dependent
    /// check is `false` and the regime is `Rejected`, named and never hidden.
    pub fn probe() -> Host {
        #[cfg(target_os = "linux")]
        let checks = linux::LinuxChecks::detect();
        #[cfg(not(target_os = "linux"))]
        let checks = unsupported::UnsupportedChecks::detect();
        Self::probe_with(&checks)
    }

    /// Probe using an injected [`CapabilityChecks`] implementation. The real seam tests exercise
    /// to synthesize failing hosts without real KVM hardware (specs/baud-host.md §6).
    pub fn probe_with(checks: &dyn CapabilityChecks) -> Host {
        Host {
            report: checks::compute_probe(checks),
            topology: checks.topology(),
        }
    }

    /// Physical cores available for VMs: total physical cores minus housekeeping reservation.
    /// SMT adds no capacity (specs/baud-host.md §5) — sibling threads are never double-counted.
    pub fn capacity(&self) -> usize {
        self.topology
            .cores
            .len()
            .saturating_sub(self.topology.housekeeping_reserved)
    }

    /// Place `n` single-vCPU VMs, one physical core each, never splitting hyperthread siblings.
    /// Refuses (rather than oversubscribing) when `n` exceeds [`Host::capacity`].
    pub fn place(&self, n: usize) -> Result<Placement, PlacementError> {
        placement::place(&self.topology, n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use checks::test_support::*;

    /// specs/baud-host.md §6 `capacity_refuses_sibling_split` — placement never oversubscribes,
    /// and every accepted placement keeps distinct physical cores (no two VMs share an SMT pair).
    #[test]
    fn capacity_refuses_sibling_split() {
        let host = Host::probe_with(&fake_checks_ok(hyperthreaded_topology(4)));
        let plan = host.place(host.capacity() + 1);
        assert!(plan.is_err(), "placing one VM over capacity must be refused");

        let full = host.place(host.capacity()).expect("placing at capacity must succeed");
        assert!(full.no_two_on_sibling_threads());
    }

    /// specs/baud-host.md §6 `doctor_checks_kvm` — a fully-capable host reports every required
    /// check true and lands on a real regime, never `Rejected`.
    #[test]
    fn doctor_checks_kvm() {
        let host = Host::probe_with(&fake_checks_ok(hyperthreaded_topology(4)));
        assert!(host.kvm && host.vmx && host.rcb_deterministic);
        assert!(host.is_runnable());
        assert!(host.reason.is_none() || !host.is_enforced_capable());
    }

    /// specs/baud-host.md §6 `rejected_host_names_the_failing_check` — a missing `/dev/kvm`
    /// rejects the host and the reason names the failing check, never a silent/false pass.
    #[test]
    fn rejected_host_names_the_failing_check() {
        let host = Host::probe_with(&no_kvm());
        assert!(!host.is_runnable());
        let reason = host.reason.clone().expect("a rejected host must name why");
        assert!(reason.contains("/dev/kvm"), "reason was: {reason}");
    }

    #[test]
    fn amd_host_is_runnable_but_not_enforced_capable() {
        let mut checks = fake_checks_ok(single_core_topology(2));
        checks.vendor = Vendor::Amd;
        checks.enforced_module_present = true; // even "present" doesn't matter on AMD (phase-2)
        let host = Host::probe_with(&checks);
        assert!(host.is_runnable());
        assert!(!host.is_enforced_capable());
        assert!(host.reason.as_deref().unwrap_or("").contains("AMD"));
    }

    #[test]
    fn missing_msr_filter_rejects_even_with_kvm_present() {
        let mut checks = fake_checks_ok(single_core_topology(2));
        checks.msr_filter_ok = false;
        let host = Host::probe_with(&checks);
        assert!(!host.is_runnable());
        assert!(host.reason.clone().unwrap().contains("MSR filter"));
    }

    #[test]
    fn probe_report_json_shape_matches_spec() {
        let mut checks = fake_checks_ok(single_core_topology(2));
        checks.enforced_module_present = false; // stock KVM => runnable but not enforced-capable
        let host = Host::probe_with(&checks);
        let v = serde_json::to_value(&host.report).unwrap();
        for key in [
            "kvm", "vmx", "cpuid", "tsc_stable", "msr_filter", "singlestep",
            "rcb_deterministic", "nested", "vendor", "enforced_module_present",
        ] {
            assert!(v.get(key).is_some(), "missing field {key} in Probe JSON");
        }
        assert_eq!(v["enforced_module_present"], false);
        assert!(host.is_runnable());
        assert!(!host.is_enforced_capable());
    }
}
