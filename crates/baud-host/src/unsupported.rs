// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The non-Linux capability-check implementation. baud-multiverse owns the guest through
// `/dev/kvm`, which only exists on Linux; a host running any other OS — this dev machine's
// Windows unless it is a Linux/WSL2 box with KVM wired through — genuinely cannot run baud.
// Every check reports honestly rather than guessing, so `Host::probe()` rejects and names the
// reason instead of silently pretending success (specs/baud-host.md §3: "a failure downgrades
// the regime and is recorded, never hidden").

use crate::{CapabilityChecks, CoreTopology, Topology, Vendor};

pub struct UnsupportedChecks {
    physical_cores: usize,
}

impl UnsupportedChecks {
    pub fn detect() -> Self {
        // Best-effort logical CPU count so `capacity()`/`place()` are at least self-consistent
        // even though `Regime::Rejected` means no VM will ever actually be placed here. We cannot
        // tell physical cores from logical ones without OS-specific topology APIs this module
        // deliberately does not implement (real placement is only meaningful on Linux).
        let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        Self { physical_cores: logical.max(1) }
    }
}

impl CapabilityChecks for UnsupportedChecks {
    fn kvm_present(&self) -> bool {
        false
    }

    fn vmx_present(&self) -> bool {
        false
    }

    fn vendor(&self) -> Vendor {
        Vendor::Other
    }

    fn cpuid_control_ok(&self) -> bool {
        false
    }

    fn tsc_stable(&self) -> bool {
        false
    }

    fn msr_filter_ok(&self) -> bool {
        false
    }

    fn singlestep_ok(&self) -> bool {
        false
    }

    fn rcb_deterministic(&self) -> bool {
        false
    }

    fn nested_virt(&self) -> bool {
        false
    }

    fn enforced_module_present(&self) -> bool {
        false
    }

    fn topology(&self) -> Topology {
        // One "core" per logical CPU is a placeholder shape only: on this OS no VM can ever be
        // placed (regime is always Rejected), so no accuracy claim is made about SMT siblings.
        let cores = (0..self.physical_cores)
            .map(|i| CoreTopology { physical_id: i, sibling_threads: vec![i] })
            .collect();
        Topology { cores, housekeeping_reserved: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Regime;

    #[test]
    fn unsupported_os_is_rejected_and_names_kvm() {
        let checks = UnsupportedChecks::detect();
        let host = crate::Host::probe_with(&checks);
        assert_eq!(host.regime, Regime::Rejected);
        assert!(host.reason.clone().unwrap().contains("/dev/kvm"));
    }
}
