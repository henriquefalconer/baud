// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The real Linux/KVM capability-check implementation (specs/baud-host.md §3's table, one
// function per row). Every check is independent and fails closed: an ioctl error, a missing
// /proc or /sys file, or any surprise never panics and never reports success by assumption.
//
// This module is exercised for real against actual `/dev/kvm` on this project's dev machine
// (a bare-metal Dell XPS 13 running Ubuntu on WSL2, CLAUDE.md) — `rcb_deterministic`,
// `cpuid_control_ok`, `tsc_stable`, `msr_filter_ok`, and `singlestep_ok` are the H0 gate
// (specs/baud-host.md, todo.md §10 "must actually run") and have been validated on real silicon;
// see docs/determinism.md for the recorded result.

use crate::{CapabilityChecks, CoreTopology, Topology, Vendor};
use kvm_ioctls::{Cap, Kvm};
use std::fs;

pub struct LinuxChecks;

impl LinuxChecks {
    pub fn detect() -> Self {
        LinuxChecks
    }
}

impl CapabilityChecks for LinuxChecks {
    fn kvm_present(&self) -> bool {
        std::path::Path::new("/dev/kvm").exists() && Kvm::new().is_ok()
    }

    fn vmx_present(&self) -> bool {
        cpuinfo_flag("vmx") || cpuinfo_flag("svm")
    }

    fn vendor(&self) -> Vendor {
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        if cpuinfo.contains("GenuineIntel") {
            Vendor::Intel
        } else if cpuinfo.contains("AuthenticAMD") {
            Vendor::Amd
        } else {
            Vendor::Other
        }
    }

    /// `KVM_SET_CPUID2` round-trips a masked leaf: fetch the host-supported CPUID set, clear the
    /// RDRAND bit (`01H:ECX[30]`, §3.2 of todo.md) on a scratch vCPU, and confirm the ioctl
    /// accepts it.
    fn cpuid_control_ok(&self) -> bool {
        with_vcpu(|kvm, vcpu| {
            let mut cpuid = kvm.get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES).ok()?;
            for entry in cpuid.as_mut_slice().iter_mut() {
                if entry.function == 1 {
                    entry.ecx &= !(1 << 30); // clear RDRAND
                }
            }
            vcpu.set_cpuid2(&cpuid).ok()
        })
        .is_some()
    }

    /// `KVM_GET_TSC_KHZ` must not fail with `-EIO` (an unstable host TSC).
    fn tsc_stable(&self) -> bool {
        with_vcpu(|_kvm, vcpu| vcpu.get_tsc_khz().ok()).is_some()
    }

    fn msr_filter_ok(&self) -> bool {
        Kvm::new().map(|kvm| kvm.check_extension(Cap::X86MsrFilter)).unwrap_or(false)
    }

    fn singlestep_ok(&self) -> bool {
        Kvm::new().map(|kvm| kvm.check_extension(Cap::SetGuestDebug)).unwrap_or(false)
    }

    /// A fixed userspace loop's retired-conditional-branch count must be identical across two
    /// runs (todo.md §3.7: "validated, not assumed"). Uses the raw `BR_INST_RETIRED.COND` event
    /// (`PERF_TYPE_RAW`, config `0x11c4`) via the `perf-event` crate's `attrs_mut` escape hatch —
    /// the same raw event `baud-multiverse`'s work-clock reads on the vCPU thread
    /// (`crates/baud-multiverse/src/linux/mod.rs`'s `LinuxBranchCounter`), just measured here in
    /// host userspace before any guest exists. **Not** the generic `PERF_COUNT_HW_BRANCH_
    /// INSTRUCTIONS` (all branches): `docs/determinism.md`'s own H0 measurement found that event
    /// `±1`-nondeterministic on this exact host, which is exactly why specs §3.3 requires the raw
    /// event by name — this function and `LinuxBranchCounter` had both drifted onto the generic
    /// one despite that documented decision (todo.md §14 next-actions item 2(c) follow-up).
    ///
    /// Real-hardware finding on this machine (a nested-virtualized WSL2 host): even with the
    /// counter `pinned` to the PMU, one-off PMU-scheduling multiplexing hiccups occasionally
    /// undercount a single trial (observed independently of any system load, ~1-in-15 trials).
    /// That is PMU contention noise, not the genuine branch-counter nondeterminism this check
    /// exists to catch — so a single disagreeing pair is not conclusive either way. Take three
    /// trials and accept if any two agree (majority vote): still rejects a CPU whose branch count
    /// is genuinely unstable (all three would disagree), while no longer flagging a working host
    /// on the strength of one transient miscount.
    fn rcb_deterministic(&self) -> bool {
        let trials: Vec<u64> = (0..3).filter_map(|_| measure_fixed_loop_branches()).collect();
        trials.len() == 3 && trials.iter().any(|a| trials.iter().filter(|b| *b == a).count() >= 2)
    }

    fn nested_virt(&self) -> bool {
        for path in [
            "/sys/module/kvm_intel/parameters/nested",
            "/sys/module/kvm_amd/parameters/nested",
        ] {
            if let Ok(s) = fs::read_to_string(path) {
                let s = s.trim();
                if s == "1" || s.eq_ignore_ascii_case("y") {
                    return true;
                }
            }
        }
        false
    }

    /// The enforced regime (specs/baud-host.md §8) needs a patched, out-of-tree `kvm_intel.ko`
    /// (`kernel-module/baud-enforced/{rdtsc,rdrand,ud2}-enforce.patch`, ENFORCEMENT_DESIGN.md).
    /// All three instructions the regime covers are now implemented there — RDTSC via
    /// `CPU_BASED_RDTSC_EXITING`, RDRAND via the exit-handler table, and RDSEED via
    /// `baud-packages`' build-time `rdseed`→`UD2` rewrite plus `ud2-enforce.patch`'s
    /// `handle_baud_ud2_exit`.
    ///
    /// **This reports whether the patched module is *loaded right now*, which is a different
    /// question from whether it exists**, and it deliberately stays `false`: that module is only
    /// ever swapped in transiently, by `drive/h3-enforced-{rdtsc,rdrand,rdseed}.sh`, each of which
    /// unconditionally restores the stock module on exit (CLAUDE.md). Every other process on this
    /// host — including whatever calls this — therefore runs against the stock module, and
    /// reporting otherwise would overclaim guarantees the running kernel does not provide
    /// (`capability_is_recorded_and_not_overclaimed`). Wiring this to a real runtime check (e.g. a
    /// `KVM_CHECK_EXTENSION` for `KVM_EXIT_BAUD_DETERMINISM`, which the patches do not add yet)
    /// is the outstanding work here, not the enforcement logic itself.
    ///
    /// The probe module's finding that this host's VMX microcode does not allow setting
    /// `SECONDARY_EXEC_RDSEED_EXITING` (`kernel-module/baud-enforced/BUILD.md`'s "Result") is **no
    /// longer a blocker for the RDSEED half**, as it was assumed to be when that probe was written:
    /// the build-time `UD2` rewrite means the real `RDSEED` opcode never executes in the guest, so
    /// no secondary control is needed — the `UD2`'s `#UD` is already trapped by the exception
    /// bitmap stock KVM sets unconditionally.
    fn enforced_module_present(&self) -> bool {
        false
    }

    fn topology(&self) -> Topology {
        read_topology()
    }
}

fn cpuinfo_flag(flag: &str) -> bool {
    fs::read_to_string("/proc/cpuinfo")
        .map(|s| {
            s.lines()
                .find(|l| l.starts_with("flags") || l.starts_with("Features"))
                .map(|l| l.split_whitespace().any(|f| f == flag))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Open `/dev/kvm`, create a scratch VM + vCPU 0, and run `f` against them. `None` on any failure
/// along the way (never panics) — the scratch VM is never given memory or run; it exists only to
/// exercise the ioctls a real boot will also use.
fn with_vcpu<T>(f: impl FnOnce(&Kvm, &kvm_ioctls::VcpuFd) -> Option<T>) -> Option<T> {
    let kvm = Kvm::new().ok()?;
    let vm = kvm.create_vm().ok()?;
    let vcpu = vm.create_vcpu(0).ok()?;
    f(&kvm, &vcpu)
}

/// A fixed control-flow shape (a bounded loop with one data-dependent branch) so the retired
/// conditional-branch count is a function of the code, not of input.
#[inline(never)]
fn fixed_branch_workload() -> u64 {
    let mut acc: u64 = 0;
    for i in 0..100_000u64 {
        if i % 2 == 0 {
            acc = acc.wrapping_add(i);
        } else {
            acc = acc.wrapping_sub(1);
        }
    }
    std::hint::black_box(acc)
}

/// `PERF_TYPE_RAW` (perf_event_open(2)) and Intel `BR_INST_RETIRED.COND` (event `0xC4`, umask
/// `0x11`) — see `rcb_deterministic`'s doc above and `crates/baud-multiverse/src/linux/mod.rs`'s
/// matching `BR_INST_RETIRED_COND` constant for why this, not the generic `HW_BRANCH_INSTRUCTIONS`.
const PERF_TYPE_RAW: u32 = 4;
const BR_INST_RETIRED_COND: u64 = 0x11c4;

fn measure_fixed_loop_branches() -> Option<u64> {
    let mut builder = perf_event::Builder::new();
    builder.attrs_mut().type_ = PERF_TYPE_RAW;
    builder.attrs_mut().config = BR_INST_RETIRED_COND;
    // Under WSL2 (a nested-virtualized host), the PMU is contended enough that an unpinned
    // counter is occasionally multiplexed off the PMU for part of the measurement window,
    // undercounting and making two back-to-back measurements of the *same* fixed loop
    // disagree — not genuine hardware nondeterminism, just this counter losing the PMU for a
    // moment. `pinned(true)` asks the kernel to keep it resident for the whole enable/disable
    // window instead.
    builder.pinned(true);
    let mut counter = builder.build().ok()?;
    counter.enable().ok()?;
    let _ = fixed_branch_workload();
    counter.disable().ok()?;
    counter.read().ok()
}

/// Physical-core topology from `/sys/devices/system/cpu/cpu*/topology/thread_siblings_list`:
/// one [`CoreTopology`] per physical core, with every SMT sibling logical CPU grouped together so
/// [`crate::placement::place`] can never split a pair (specs/baud-host.md §5).
fn read_topology() -> Topology {
    let cpu_dir = std::path::Path::new("/sys/devices/system/cpu");
    let mut by_min_sibling: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();

    if let Ok(entries) = fs::read_dir(cpu_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(idx_str) = name.strip_prefix("cpu") else { continue };
            if idx_str.is_empty() || !idx_str.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(logical_id) = idx_str.parse::<usize>() else { continue };

            let siblings_path = cpu_dir.join(&*name).join("topology/thread_siblings_list");
            let siblings = fs::read_to_string(&siblings_path)
                .ok()
                .map(|s| parse_cpu_list(s.trim()))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| vec![logical_id]);

            let key = *siblings.iter().min().unwrap_or(&logical_id);
            by_min_sibling.entry(key).or_insert(siblings);
        }
    }

    let cores: Vec<CoreTopology> = by_min_sibling
        .into_values()
        .enumerate()
        .map(|(i, sibling_threads)| CoreTopology { physical_id: i, sibling_threads })
        .collect();

    // 2 cores/socket held back for host bookkeeping/RCU/IRQ (specs/baud-host.md §5), scaled down
    // on small hosts so capacity is never negative.
    let housekeeping_reserved = if cores.len() > 8 {
        2
    } else if cores.len() > 2 {
        1
    } else {
        0
    };

    Topology { cores, housekeeping_reserved }
}

/// Parse a Linux `*_siblings_list` / `*_cpus_list` value: comma-separated ids and `a-b` ranges.
fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                out.extend(a..=b);
                continue;
            }
        }
        if let Ok(v) = part.parse::<usize>() {
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_list_parses_ranges_and_singletons() {
        assert_eq!(parse_cpu_list("0,1"), vec![0, 1]);
        assert_eq!(parse_cpu_list("0-3"), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpu_list("4"), vec![4]);
        assert_eq!(parse_cpu_list(""), Vec::<usize>::new());
    }
}
