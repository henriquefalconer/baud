// SPDX-License-Identifier: GPL-2.0
//
// baud_enforced_probe — read-only VMX capability probe for the "enforced" determinism
// regime (todo.md §3.8, specs/baud-multiverse.md §3.8, specs/baud-host.md §8).
//
// The enforced regime needs the host's VMX to support forcing two VM-execution controls
// on for every guest, regardless of what the guest wants: RDTSC-exiting (primary
// proc-based control, bit 12) and RDRAND/RDSEED-exiting (secondary proc-based controls,
// bits 11/16) so an adversarial guest can never read the real timestamp counter or
// hardware entropy. Stock KVM never sets these bits, and no userspace ioctl exposes them
// (that gap is exactly why the enforced regime needs a kernel module at all, not just a
// baud-multiverse code change).
//
// This module answers the open hardware question first, before any VMX state is
// touched: does *this* CPU's microcode even allow those bits to be set at all? It reads
// the read-only VMX capability MSRs (Intel SDM Vol. 3D §A.3) and logs whether every bit
// the enforced regime will eventually need is available in the "allowed-1" settings. It
// never writes a VMX control, never touches a running vCPU, and has no interaction with
// KVM or any guest — it is safe to load on a host with VMs already running. It builds
// cleanly with the correct vermagic for this exact kernel (see BUILD.md), but `insmod`
// does not yet succeed here: a struct-module-size ABI mismatch traced to Microsoft's
// exact build toolchain (gcc 13.2.0 + binutils 2.41) vs. any Ubuntu-packaged substitute —
// see BUILD.md's "Known blocker" section, not a bug in this module. Actually setting
// these bits on live guests (the next step, once loadable) requires hooking KVM's own
// VMCS setup, which is a
// materially different, higher-risk change deliberately left out of this module.
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>
#include <asm/msr.h>
#include <asm/msr-index.h>
#include <asm/cpufeature.h>

// CPU_BASED_*/SECONDARY_EXEC_* control-bit names live in arch/x86/kvm/vmx/vmx.h,
// which is KVM-internal and not exported to out-of-tree modules — redefined here
// from the Intel SDM Vol. 3C §25.6.1/§25.6.2 bit layout instead.
#define CPU_BASED_ACTIVATE_SECONDARY_CONTROLS (1u << 31)
#define CPU_BASED_RDTSC_EXITING                (1u << 12)
#define SECONDARY_EXEC_RDRAND_EXITING          (1u << 11)
#define SECONDARY_EXEC_RDSEED_EXITING          (1u << 16)

// Allowed-1 settings live in the high 32 bits of these paired capability MSRs
// (SDM Vol. 3D §A.3.1/§A.3.3): bit N of the low half is allowed-0, bit N of the
// high half is allowed-1. A control can be set to 1 only if its allowed-1 bit is 1.
static u32 allowed1(u64 cap_msr)
{
	return (u32)(cap_msr >> 32);
}

static int __init baud_enforced_probe_init(void)
{
	u64 basic, procbased, procbased2;
	u32 proc_allowed1, sec_allowed1;
	bool has_secondary, has_rdtsc_exit, has_rdrand_exit, has_rdseed_exit;
	int err;

	if (!boot_cpu_has(X86_FEATURE_VMX)) {
		pr_warn("baud_enforced_probe: VMX not present on this CPU (boot_cpu_has(X86_FEATURE_VMX)=0); "
			"enforced regime is unreachable here\n");
		return -ENODEV;
	}

	err = rdmsrq_safe(MSR_IA32_VMX_BASIC, &basic);
	if (err) {
		pr_err("baud_enforced_probe: rdmsr(IA32_VMX_BASIC) failed: %d\n", err);
		return err;
	}

	// SDM §A.3.1: if bit 55 is set, use the TRUE_PROCBASED_CTLS MSR instead of
	// PROCBASED_CTLS for accurate allowed-1 reporting.
	err = rdmsrq_safe((basic & (1ULL << 55)) ? MSR_IA32_VMX_TRUE_PROCBASED_CTLS
						   : MSR_IA32_VMX_PROCBASED_CTLS,
			   &procbased);
	if (err) {
		pr_err("baud_enforced_probe: rdmsr(IA32_VMX_PROCBASED_CTLS) failed: %d\n", err);
		return err;
	}

	proc_allowed1 = allowed1(procbased);
	has_secondary = proc_allowed1 & CPU_BASED_ACTIVATE_SECONDARY_CONTROLS;
	has_rdtsc_exit = proc_allowed1 & CPU_BASED_RDTSC_EXITING;

	has_rdrand_exit = false;
	has_rdseed_exit = false;
	if (has_secondary) {
		err = rdmsrq_safe(MSR_IA32_VMX_PROCBASED_CTLS2, &procbased2);
		if (err) {
			pr_err("baud_enforced_probe: rdmsr(IA32_VMX_PROCBASED_CTLS2) failed: %d\n", err);
			return err;
		}
		sec_allowed1 = allowed1(procbased2);
		has_rdrand_exit = sec_allowed1 & SECONDARY_EXEC_RDRAND_EXITING;
		has_rdseed_exit = sec_allowed1 & SECONDARY_EXEC_RDSEED_EXITING;
	}

	pr_info("baud_enforced_probe: VMX capability report for the enforced regime "
		"(todo.md \"enforced-regime KVM module\"):\n");
	pr_info("baud_enforced_probe:   RDTSC-exiting settable:          %s\n",
		has_rdtsc_exit ? "yes" : "NO");
	pr_info("baud_enforced_probe:   secondary controls available:    %s\n",
		has_secondary ? "yes" : "NO");
	pr_info("baud_enforced_probe:   RDRAND-exiting settable:         %s\n",
		has_rdrand_exit ? "yes" : "NO");
	pr_info("baud_enforced_probe:   RDSEED-exiting settable:         %s\n",
		has_rdseed_exit ? "yes" : "NO");
	pr_info("baud_enforced_probe:   enforced regime hardware-feasible on this CPU: %s\n",
		(has_rdtsc_exit && has_rdrand_exit && has_rdseed_exit) ? "YES" : "no");

	return 0;
}

static void __exit baud_enforced_probe_exit(void)
{
	pr_info("baud_enforced_probe: unloaded (read-only probe, no state to tear down)\n");
}

module_init(baud_enforced_probe_init);
module_exit(baud_enforced_probe_exit);

MODULE_LICENSE("GPL");
MODULE_AUTHOR("baud");
MODULE_DESCRIPTION("Read-only VMX capability probe for the baud enforced determinism regime");
MODULE_VERSION("0.1.0");
