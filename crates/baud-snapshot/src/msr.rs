// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The three TSC-family MSR numbers the cooperative regime routes to the VMM (specs/
// baud-multiverse.md §4's MSR-filter row, todo.md §3.3) and that `universe::order_msrs_tsc_first`
// below sequences on restore (specs/baud-snapshot.md §6: "Restore `IA32_TSC` before
// `IA32_TSC_DEADLINE`"). Single source of truth for both crates that need them —
// `baud-multiverse::timesource` re-exports these rather than redefining them, since
// `baud-multiverse` depends on `baud-snapshot` (specs/baud-snapshot.md §2's architecture diagram),
// not the other way around.

pub const MSR_IA32_TSC: u32 = 0x0000_0010;
pub const MSR_IA32_TSC_DEADLINE: u32 = 0x0000_06E0;
pub const MSR_IA32_TSC_AUX: u32 = 0xC000_0103;
