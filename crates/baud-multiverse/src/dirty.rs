// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The pure half of wiring `baud-snapshot`'s `KVM_CAP_DIRTY_LOG_RING`-based reset into
// `Multiverse` (specs/baud-snapshot.md §5: "rewind copies back only dirtied pages ... cost ∝
// change, not machine size"; todo.md §14's "DirtyRing is not yet wired into baud-multiverse's
// Multiverse" gap, closed by this module + `linux::Multiverse::{enable_dirty_ring,
// reset_dirty_pages}`).
//
// Deliberately NOT inside `linux/mod.rs`: that module is `#[cfg(target_os = "linux")]`-gated
// (`lib.rs`), so on this Windows dev machine `cargo test --workspace` never compiles it at all —
// only `cargo check --target x86_64-unknown-linux-gnu` type-checks it (CLAUDE.md, todo.md §14's
// running theme). The one piece of this wiring with real logic to get wrong — mapping a
// `DirtyRing::collect()` harvest down to the RAM page indices a rewind must restore — has no KVM
// or mmap dependency whatsoever, so it lives here, ungated, where `cargo test -p baud-multiverse`
// actually exercises it on every platform (same split `baud-snapshot::dirty_ring` vs.
// `baud-snapshot::linux::DirtyRing` already uses one crate down).

/// The KVM memslot number the guest-RAM region is registered under
/// (`linux::allocate_and_register_guest_ram`'s `region.slot = 0`). Exactly one memslot exists in
/// this workspace today — todo.md §1's one-vCPU-per-VM constraint means there is never a second
/// vCPU's memory to separately track — but [`ram_page_indices`] still filters on it explicitly
/// rather than assuming every harvested entry belongs to RAM, so a future second memslot (e.g.
/// MMIO-backed device memory gaining its own dirty tracking) fails closed instead of silently
/// misinterpreting a non-RAM slot's page offset as a RAM page index.
pub const RAM_SLOT: u32 = 0;

/// Reduce `DirtyRing::collect()`'s harvested `(slot, offset)` pairs (`baud_snapshot::linux::
/// DirtyRing::collect`) down to the RAM page indices a rewind must restore, keeping only entries
/// for `ram_slot` and discarding the rest.
///
/// `offset` is the kernel's page number *within the slot* — `kvm_dirty_gfn`'s documented field,
/// mirrored in `baud_snapshot::dirty_ring::RawDirtyGfn` and unit-tested there independent of this
/// crate. Because the RAM memslot is registered starting at guest-physical address
/// `layout::GUEST_RAM_START` with every page exactly `baud_snapshot::PAGE_SIZE` bytes and no gaps
/// (`linux::allocate_and_register_guest_ram`, one contiguous region), `offset` *is* directly the
/// RAM page index: page `offset` covers guest-physical
/// `[GUEST_RAM_START + offset * PAGE_SIZE, GUEST_RAM_START + (offset + 1) * PAGE_SIZE)`, the exact
/// same indexing `universe::Universe::ram` already uses (`universe.rs`'s doc: "page `i` covers
/// guest-physical `[i * PAGE_SIZE, (i+1) * PAGE_SIZE)`") — no further scaling needed, and a
/// harvested page index can be used directly as an index into a captured `Universe`'s `ram` slice.
pub fn ram_page_indices(harvested: &[(u32, u64)], ram_slot: u32) -> Vec<usize> {
    harvested.iter().filter(|(slot, _)| *slot == ram_slot).map(|(_, offset)| *offset as usize).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_entries_matching_the_ram_slot() {
        let harvested = vec![(0, 5), (1, 99), (0, 7)];
        assert_eq!(ram_page_indices(&harvested, 0), vec![5, 7]);
    }

    #[test]
    fn a_non_ram_slot_entry_is_dropped_not_misread_as_a_page_index() {
        // slot 1 (hypothetical future device memslot) at a huge offset must never leak through as
        // if it were RAM page 4096 -- only slot 0 (RAM_SLOT) entries survive.
        let harvested = vec![(1, 4096)];
        assert!(ram_page_indices(&harvested, RAM_SLOT).is_empty());
    }

    #[test]
    fn empty_harvest_yields_no_pages() {
        assert!(ram_page_indices(&[], RAM_SLOT).is_empty());
    }

    #[test]
    fn preserves_harvest_order_within_the_kept_slot() {
        let harvested = vec![(0, 9), (0, 3), (0, 1)];
        assert_eq!(ram_page_indices(&harvested, 0), vec![9, 3, 1], "order must match the ring's harvest order, not be re-sorted");
    }

    proptest::proptest! {
        /// specs/baud-snapshot.md §5's guarantee restated at this layer: the number of RAM pages a
        /// rewind restores never exceeds the number of entries the dirty ring actually harvested
        /// (it can only be less, if some harvested entries belong to a different slot) --
        /// filtering can only shrink the set, never invent pages that were not dirtied.
        #[test]
        fn output_length_never_exceeds_harvested_length(
            entries in proptest::collection::vec((0u32..3, 0u64..1000), 0..200)
        ) {
            let out = ram_page_indices(&entries, 0);
            proptest::prop_assert!(out.len() <= entries.len());
        }

        /// Every returned index came from a slot-0 entry's offset, verbatim (no arithmetic
        /// transform beyond the filter) -- pins down that this function does exactly one thing:
        /// select, not rescale.
        #[test]
        fn every_returned_index_is_a_verbatim_ram_slot_offset(
            entries in proptest::collection::vec((0u32..3, 0u64..1000), 0..200)
        ) {
            let expected: Vec<usize> = entries.iter().filter(|(s, _)| *s == 0).map(|(_, o)| *o as usize).collect();
            proptest::prop_assert_eq!(ram_page_indices(&entries, 0), expected);
        }
    }
}
