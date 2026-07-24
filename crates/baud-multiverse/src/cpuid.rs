// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The CPUID determinism mask table (specs/baud-multiverse.md §4, todo.md §3.2's "mask table").
//
// Under VT-x, `cpuid` always exits — the VMM owns every leaf. This module is the pure masking
// logic that turns "whatever KVM_GET_SUPPORTED_CPUID reports the host CPU can do" into "the fixed,
// nondeterminism-free leaf set every baud guest sees", independent of any KVM ioctl so it is
// unit-testable on any OS (including this Windows dev machine, which has no `/dev/kvm`). The
// `linux` module applies this exact same function to the real `kvm_cpuid_entry2` payload before
// `KVM_SET_CPUID2` — see `linux::apply_cpuid_mask`.

/// The six raw fields the determinism mask reads or writes on a CPUID leaf. Implemented for a
/// portable [`CpuidLeaf`] (used by the tests below) and, in `linux/mod.rs`, directly for
/// `kvm_bindings::kvm_cpuid_entry2` — so the exact masking logic exercised here runs unmodified
/// against the real `KVM_SET_CPUID2` payload.
pub trait CpuidEntry {
    fn function(&self) -> u32;
    fn index(&self) -> u32;
    fn eax(&self) -> u32;
    fn set_eax(&mut self, v: u32);
    fn ebx(&self) -> u32;
    fn set_ebx(&mut self, v: u32);
    fn ecx(&self) -> u32;
    fn set_ecx(&mut self, v: u32);
    fn edx(&self) -> u32;
    fn set_edx(&mut self, v: u32);
}

/// A portable stand-in for `kvm_bindings::kvm_cpuid_entry2`'s six masked fields — lets
/// `apply_determinism_mask` be unit-tested without linking KVM at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuidLeaf {
    pub function: u32,
    pub index: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

impl CpuidEntry for CpuidLeaf {
    fn function(&self) -> u32 {
        self.function
    }
    fn index(&self) -> u32 {
        self.index
    }
    fn eax(&self) -> u32 {
        self.eax
    }
    fn set_eax(&mut self, v: u32) {
        self.eax = v;
    }
    fn ebx(&self) -> u32 {
        self.ebx
    }
    fn set_ebx(&mut self, v: u32) {
        self.ebx = v;
    }
    fn ecx(&self) -> u32 {
        self.ecx
    }
    fn set_ecx(&mut self, v: u32) {
        self.ecx = v;
    }
    fn edx(&self) -> u32 {
        self.edx
    }
    fn set_edx(&mut self, v: u32) {
        self.edx = v;
    }
}

// Leaf/bit constants named after the spec bullets they implement (todo.md §3.2, specs/
// baud-multiverse.md §4). Keeping the numbers here, not scattered through the masking match arms,
// is what makes `cpuid_leaves_are_fixed`'s "assert RDRAND/RDSEED/TSX/x2APIC bits are 0" readable.
const LEAF_FEATURES: u32 = 0x01; // RDRAND (ECX[30]), x2APIC (ECX[21]), hypervisor-present (ECX[31])
const LEAF_EXTENDED_FEATURES: u32 = 0x07; // RDSEED (EBX[18]), TSX HLE (EBX[4]) / RTM (EBX[11])
const LEAF_EXTENDED_TOPOLOGY_V1: u32 = 0x0B;
const LEAF_EXTENDED_TOPOLOGY_V2: u32 = 0x1F;
const LEAF_EXTENDED_POWER_MGMT: u32 = 0x8000_0007; // invariant TSC (EDX[8])
const LEAF_TSC_CRYSTAL: u32 = 0x15; // TSC/core-crystal-clock ratio (EAX=denom, EBX=numer, ECX=Hz)
const LEAF_PROCESSOR_FREQ: u32 = 0x16; // base/max/bus MHz (EAX/EBX/ECX)

/// The nominal core-crystal-clock frequency (Hz) this table synthesizes into CPUID leaf 15H,
/// matching `linux::VIRTUAL_TSC_KHZ` exactly (that constant is defined as `TSC_CRYSTAL_HZ / 1000`
/// so the two never drift apart — one numeric source of truth for "what frequency does baud's
/// virtual TSC run at").
pub const TSC_CRYSTAL_HZ: u32 = 1_000_000_000; // 1 GHz
/// Leaf 16H's base-frequency field is reported in MHz, not Hz — derived from [`TSC_CRYSTAL_HZ`]
/// (not a second independently-chosen number) for the same reason as `VIRTUAL_TSC_KHZ`.
pub const PROCESSOR_BASE_MHZ: u32 = TSC_CRYSTAL_HZ / 1_000_000;

const ECX_RDRAND_BIT: u32 = 30;
const ECX_X2APIC_BIT: u32 = 21;
const ECX_HYPERVISOR_PRESENT_BIT: u32 = 31;
const EBX_RDSEED_BIT: u32 = 18;
const EBX_TSX_HLE_BIT: u32 = 4;
const EBX_TSX_RTM_BIT: u32 = 11;
const EDX_INVARIANT_TSC_BIT: u32 = 8;

/// Topology sub-leaf level types (Intel SDM Vol. 2A, CPUID.(EAX=0BH/1FH,ECX=n):ECX[15:8]):
/// `0` = invalid (enumeration stops here), `1` = SMT, `2` = Core.
const TOPOLOGY_LEVEL_INVALID: u32 = 0;
const TOPOLOGY_LEVEL_SMT: u32 = 1;
const TOPOLOGY_LEVEL_CORE: u32 = 2;

/// Apply the determinism mask table to every served CPUID leaf (specs/baud-multiverse.md §4,
/// todo.md §3.2): clear RDRAND/RDSEED/TSX-HLE/TSX-RTM/x2APIC, pin the extended-topology leaves to
/// one core, set the invariant-TSC bit and a fixed hypervisor-present bit, and synthesize the
/// TSC/crystal-clock ratio leaf (15H) to a fixed value host-independent of whatever (if anything)
/// the real CPU reports there. Every other leaf this table does not recognize is left untouched —
/// masking is purely subtractive/pinning, never adds a leaf that was not already present (leaf
/// presence/absence is otherwise `KVM_GET_SUPPORTED_CPUID`'s job) — 15H is the sole, deliberate
/// exception: KVM includes it in the supported set on every observed host (present, just often
/// all-zero), so this only ever overwrites values on an already-present entry, exactly like every
/// other row here.
///
/// Pure and total: same input entries always produce the same output (`mask_is_deterministic`),
/// which is what makes a served leaf reproducible across the two runs `cpuid_leaves_are_fixed`
/// compares.
pub fn apply_determinism_mask<E: CpuidEntry>(entries: &mut [E]) {
    for entry in entries.iter_mut() {
        match entry.function() {
            LEAF_FEATURES if entry.index() == 0 => {
                entry.set_ecx(clear_bit(entry.ecx(), ECX_RDRAND_BIT));
                entry.set_ecx(clear_bit(entry.ecx(), ECX_X2APIC_BIT));
                entry.set_ecx(set_bit(entry.ecx(), ECX_HYPERVISOR_PRESENT_BIT));
            }
            LEAF_EXTENDED_FEATURES if entry.index() == 0 => {
                entry.set_ebx(clear_bit(entry.ebx(), EBX_RDSEED_BIT));
                entry.set_ebx(clear_bit(entry.ebx(), EBX_TSX_HLE_BIT));
                entry.set_ebx(clear_bit(entry.ebx(), EBX_TSX_RTM_BIT));
            }
            LEAF_EXTENDED_TOPOLOGY_V1 | LEAF_EXTENDED_TOPOLOGY_V2 => {
                pin_topology_sub_leaf(entry);
            }
            LEAF_EXTENDED_POWER_MGMT if entry.index() == 0 => {
                entry.set_edx(set_bit(entry.edx(), EDX_INVARIANT_TSC_BIT));
            }
            LEAF_TSC_CRYSTAL if entry.index() == 0 => {
                // Denominator/numerator 1/1 (frequency = crystal Hz, unscaled) + a fixed crystal
                // Hz: Linux's `native_calibrate_tsc()` (arch/x86/kernel/tsc.c) trusts this leaf
                // whenever it is non-zero and returns immediately — skipping every other
                // calibration path, in particular `quick_pit_calibrate()`'s busy-poll of PIT
                // channel 2 (port 0x42), which hangs forever on baud's subtractive-rule machine
                // (no PIT is ever emulated). Discovered as a real guest hang on the first real
                // boot this crate was ever exercised against on actual KVM hardware — every
                // previous iteration only `cargo check`'d this code, which cannot see a guest
                // spin forever waiting on an unemulated device.
                entry.set_eax(1);
                entry.set_ebx(1);
                entry.set_ecx(TSC_CRYSTAL_HZ);
                entry.set_edx(0); // reserved
            }
            LEAF_PROCESSOR_FREQ if entry.index() == 0 => {
                // A *second*, independent early-boot calibration path
                // (`native_calibrate_cpu_early()`, arch/x86/kernel/tsc.c) tries this leaf (via
                // `cpu_khz_from_cpuid()`) before falling back to the same unemulated-PIT
                // `quick_pit_calibrate()` leaf 15H's fix above was written to avoid — synthesizing
                // 15H alone was not sufficient to stop every PIT-polling path; this leaf needed
                // the identical treatment, discovered only because the guest kept hanging on port
                // 0x42 even after the first fix landed. `cpu_khz_from_cpuid()` only requires a
                // non-zero base-MHz (EAX); base/max are set equal (no boost state to model) and
                // bus MHz (ECX) is an arbitrary conventional value no calibration path reads.
                entry.set_eax(PROCESSOR_BASE_MHZ);
                entry.set_ebx(PROCESSOR_BASE_MHZ);
                entry.set_ecx(100);
                entry.set_edx(0);
            }
            _ => {}
        }
    }
}

/// Pin one extended-topology (0BH/1FH) sub-leaf to a single-core, single-thread, no-SMT machine:
/// sub-leaf 0 (SMT level) and sub-leaf 1 (core level) each report exactly one logical processor
/// (`EBX = 1`); every sub-leaf beyond that is marked invalid (`ECX[15:8] = 0`) so a compliant
/// guest's enumeration loop terminates instead of reading fabricated deeper levels. `EDX` (x2APIC
/// ID) is fixed to `0` — the single vCPU is always APIC ID 0 (todo.md §1's "one virtual CPU per
/// VM").
fn pin_topology_sub_leaf<E: CpuidEntry>(entry: &mut E) {
    match entry.index() {
        0 => {
            entry.set_eax(0); // shift needed to get the next-level x2APIC ID = 0 (one thread)
            entry.set_ebx(1); // one logical processor at the SMT level
            entry.set_ecx((entry.index() & 0xFF) | (TOPOLOGY_LEVEL_SMT << 8));
            entry.set_edx(0);
        }
        1 => {
            entry.set_eax(0);
            entry.set_ebx(1); // one logical processor at the core level
            entry.set_ecx((entry.index() & 0xFF) | (TOPOLOGY_LEVEL_CORE << 8));
            entry.set_edx(0);
        }
        n => {
            entry.set_eax(0);
            entry.set_ebx(0);
            entry.set_ecx((n & 0xFF) | (TOPOLOGY_LEVEL_INVALID << 8));
            entry.set_edx(0);
        }
    }
}

fn clear_bit(value: u32, bit: u32) -> u32 {
    value & !(1u32 << bit)
}

fn set_bit(value: u32, bit: u32) -> u32 {
    value | (1u32 << bit)
}

fn bit_is_set(value: u32, bit: u32) -> bool {
    value & (1u32 << bit) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A leaf reporting every masked feature as *supported* — the worst case
    /// `KVM_GET_SUPPORTED_CPUID` can hand back — must still come out fully masked.
    fn all_bits_set_leaf(function: u32, index: u32) -> CpuidLeaf {
        CpuidLeaf { function, index, eax: u32::MAX, ebx: u32::MAX, ecx: u32::MAX, edx: u32::MAX }
    }

    #[test]
    fn rdrand_x2apic_are_cleared_and_hypervisor_present_is_set_on_leaf_1() {
        let mut entries = [all_bits_set_leaf(LEAF_FEATURES, 0)];
        apply_determinism_mask(&mut entries);
        let ecx = entries[0].ecx;
        assert!(!bit_is_set(ecx, ECX_RDRAND_BIT), "RDRAND (01H:ECX[30]) must be cleared");
        assert!(!bit_is_set(ecx, ECX_X2APIC_BIT), "x2APIC (01H:ECX[21]) must be cleared");
        assert!(bit_is_set(ecx, ECX_HYPERVISOR_PRESENT_BIT), "hypervisor-present must be fixed on");
    }

    #[test]
    fn rdseed_and_tsx_are_cleared_on_leaf_7() {
        let mut entries = [all_bits_set_leaf(LEAF_EXTENDED_FEATURES, 0)];
        apply_determinism_mask(&mut entries);
        let ebx = entries[0].ebx;
        assert!(!bit_is_set(ebx, EBX_RDSEED_BIT), "RDSEED (07H:EBX[18]) must be cleared");
        assert!(!bit_is_set(ebx, EBX_TSX_HLE_BIT), "TSX HLE (07H:EBX[4]) must be cleared");
        assert!(!bit_is_set(ebx, EBX_TSX_RTM_BIT), "TSX RTM (07H:EBX[11]) must be cleared");
    }

    #[test]
    fn invariant_tsc_is_set_on_leaf_80000007h_even_if_host_lacks_it() {
        // A leaf starting fully zeroed (as if the host reported no invariant TSC at all) must
        // still come out with the bit set — baud always claims/serves an invariant virtual TSC,
        // it does not merely pass through host support (specs/baud-multiverse.md §4).
        let mut entries = [CpuidLeaf { function: LEAF_EXTENDED_POWER_MGMT, ..Default::default() }];
        apply_determinism_mask(&mut entries);
        assert!(bit_is_set(entries[0].edx, EDX_INVARIANT_TSC_BIT));
    }

    #[test]
    fn topology_leaves_are_pinned_to_one_core_no_smt() {
        for leaf_fn in [LEAF_EXTENDED_TOPOLOGY_V1, LEAF_EXTENDED_TOPOLOGY_V2] {
            let mut entries: Vec<CpuidLeaf> =
                (0..4).map(|i| all_bits_set_leaf(leaf_fn, i)).collect();
            apply_determinism_mask(&mut entries);

            // Sub-leaf 0 (SMT level): exactly one logical processor, level type SMT.
            assert_eq!(entries[0].ebx, 1);
            assert_eq!((entries[0].ecx >> 8) & 0xFF, TOPOLOGY_LEVEL_SMT);
            // Sub-leaf 1 (core level): exactly one logical processor, level type Core.
            assert_eq!(entries[1].ebx, 1);
            assert_eq!((entries[1].ecx >> 8) & 0xFF, TOPOLOGY_LEVEL_CORE);
            // Anything deeper terminates enumeration (level type invalid) and reports zero.
            for entry in &entries[2..] {
                assert_eq!(entry.ebx, 0);
                assert_eq!((entry.ecx >> 8) & 0xFF, TOPOLOGY_LEVEL_INVALID);
            }
            // The single vCPU is always x2APIC ID 0 at every sub-leaf.
            assert!(entries.iter().all(|e| e.edx == 0));
        }
    }

    #[test]
    fn unrecognized_leaves_pass_through_unmodified() {
        let original = CpuidLeaf { function: 0x2, index: 0, eax: 1, ebx: 2, ecx: 3, edx: 4 };
        let mut entries = [original];
        apply_determinism_mask(&mut entries);
        assert_eq!(entries[0], original, "masking must not touch leaves outside the mask table");
    }

    #[test]
    fn mask_is_deterministic_across_repeated_application() {
        // Applying the mask twice must be a no-op the second time: the output of a masked leaf,
        // fed back in, is a fixed point. This is what makes served CPUID reproducible across the
        // two runs `cpuid_leaves_are_fixed` compares — the mask is not accumulating state.
        let mut entries: Vec<CpuidLeaf> = vec![
            all_bits_set_leaf(LEAF_FEATURES, 0),
            all_bits_set_leaf(LEAF_EXTENDED_FEATURES, 0),
            all_bits_set_leaf(LEAF_EXTENDED_TOPOLOGY_V1, 0),
            all_bits_set_leaf(LEAF_EXTENDED_TOPOLOGY_V1, 1),
            all_bits_set_leaf(LEAF_EXTENDED_POWER_MGMT, 0),
        ];
        apply_determinism_mask(&mut entries);
        let once = entries.clone();
        apply_determinism_mask(&mut entries);
        assert_eq!(once, entries);
    }

    proptest! {
        /// However the host's real CPUID happens to look, the masked bits always come out fixed
        /// — this is `cpuid_leaves_are_fixed`'s "read every served leaf twice ... assert
        /// identical" guarantee, generalized to any starting bit pattern instead of one fixture.
        #[test]
        fn masked_bits_are_always_fixed_regardless_of_host_input(
            eax in any::<u32>(), ebx in any::<u32>(), ecx in any::<u32>(), edx in any::<u32>(),
        ) {
            let mut leaf1 = [CpuidLeaf { function: LEAF_FEATURES, index: 0, eax, ebx, ecx, edx }];
            apply_determinism_mask(&mut leaf1);
            prop_assert!(!bit_is_set(leaf1[0].ecx, ECX_RDRAND_BIT));
            prop_assert!(!bit_is_set(leaf1[0].ecx, ECX_X2APIC_BIT));
            prop_assert!(bit_is_set(leaf1[0].ecx, ECX_HYPERVISOR_PRESENT_BIT));

            let mut leaf7 = [CpuidLeaf { function: LEAF_EXTENDED_FEATURES, index: 0, eax, ebx, ecx, edx }];
            apply_determinism_mask(&mut leaf7);
            prop_assert!(!bit_is_set(leaf7[0].ebx, EBX_RDSEED_BIT));
            prop_assert!(!bit_is_set(leaf7[0].ebx, EBX_TSX_HLE_BIT));
            prop_assert!(!bit_is_set(leaf7[0].ebx, EBX_TSX_RTM_BIT));

            let mut leaf80000007 = [CpuidLeaf { function: LEAF_EXTENDED_POWER_MGMT, index: 0, eax, ebx, ecx, edx }];
            apply_determinism_mask(&mut leaf80000007);
            prop_assert!(bit_is_set(leaf80000007[0].edx, EDX_INVARIANT_TSC_BIT));
        }
    }
}
