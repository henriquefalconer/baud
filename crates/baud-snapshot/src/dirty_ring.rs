// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The pure half of `KVM_CAP_DIRTY_LOG_RING`-based reset (specs/baud-snapshot.md §5: "Track
// dirtied pages with the KVM dirty ring; rewind copies back only those pages. Cost ∝ change, not
// machine size"). This module owns *how a ring buffer full of `kvm_dirty_gfn` entries is decoded
// into a harvested dirty-page list*, independent of the real mmap/ioctl plumbing that gets such a
// buffer from the kernel in the first place (`linux::DirtyRing` does that, same split as
// `universe.rs`'s ordering logic vs. `linux.rs`'s real `KVM_GET_*`/`KVM_SET_*` calls). Every type
// and function here is hardware-independent and unit-tested on this Windows dev machine with no
// KVM/mmap involved at all.

/// A portable mirror of `kvm_bindings::kvm_dirty_gfn` (`#[repr(C)] { flags: u32, slot: u32,
/// offset: u64 }`, 16 bytes) — lets the ring-scanning protocol below be tested without the
/// `kvm-bindings` linux-only type, same rationale as `universe::MsrWrite` mirroring
/// `kvm_msr_entry`. `linux::DirtyRing` copies bytes to/from the real mmap'd `kvm_dirty_gfn` slots
/// using this exact field layout (verified by a size/order match, see that module).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawDirtyGfn {
    pub flags: u32,
    pub slot: u32,
    pub offset: u64,
}

/// Set by the kernel the instant it publishes a new dirty-page entry into the ring (Linux kernel
/// `include/uapi/linux/kvm.h`'s `KVM_DIRTY_GFN_F_DIRTY`, bit 0). A userspace scan stops at the
/// first entry *without* this bit set — that is how the reader knows it has caught up to the
/// kernel's write position without needing a separate head/tail exchange ioctl per page.
pub const DIRTY_BIT: u32 = 1 << 0;

/// Set by userspace on every entry it has harvested (`KVM_DIRTY_GFN_F_RESET`, bit 1), telling the
/// kernel "this slot's content has been consumed and its backing page may be marked clean again."
/// `KVM_RESET_DIRTY_RINGS` (the real ioctl, `linux::DirtyRing::confirm_reset`) walks every ring
/// looking for entries carrying this bit and clears them, returning how many it reset — the
/// **reset cost scales with the number of `RESET`-marked entries, not total guest RAM**, which is
/// exactly specs/baud-snapshot.md §5's guarantee (mirrors `universe::dirty_pages`'s pure half of
/// the same guarantee for the page-content-diff path).
pub const RESET_BIT: u32 = 1 << 1;

/// Scan `ring` starting at `*cursor`, collecting every `(slot, offset)` pair the kernel has
/// published since the last harvest and marking each entry's [`RESET_BIT`] in place so a
/// subsequent real `KVM_RESET_DIRTY_RINGS` ioctl knows exactly which slots to reclaim. Stops at
/// the first entry without [`DIRTY_BIT`] set (the kernel's current write position) or after at
/// most `ring.len()` entries — the latter is a defensive bound, not a correctness assumption: a
/// well-behaved kernel never lets every single ring slot carry `DIRTY` at once (it forces a soft
/// exit before the ring hard-fills), but a userspace consumer must never spin forever on
/// unexpectedly-adversarial ring content either.
///
/// `*cursor` is advanced (mod `ring.len()`) past every entry harvested, so the next call resumes
/// exactly where this one left off — repeated calls with no new kernel writes in between harvest
/// nothing (the cursor already sits on a not-yet-`DIRTY` slot).
pub fn harvest(ring: &mut [RawDirtyGfn], cursor: &mut usize) -> Vec<(u32, u64)> {
    if ring.is_empty() {
        return Vec::new();
    }
    let len = ring.len();
    *cursor %= len;
    let mut out = Vec::new();
    for _ in 0..len {
        let entry = &mut ring[*cursor];
        if entry.flags & DIRTY_BIT == 0 {
            break;
        }
        out.push((entry.slot, entry.offset));
        entry.flags |= RESET_BIT;
        *cursor = (*cursor + 1) % len;
    }
    out
}

/// The ring's byte length for `entries` slots of `kvm_dirty_gfn` (16 bytes each) — what
/// `KVM_ENABLE_CAP(KVM_CAP_DIRTY_LOG_RING, ...)`'s `args[0]` and the follow-up `mmap` length both
/// need. The kernel requires this to be a power-of-two multiple of the host page size; `entries`
/// itself being a power of two (so the modulo-wrap in [`harvest`] is a real ring) is the
/// caller-facing half of that constraint — [`linux::DirtyRing::enable`] enforces both.
pub const RAW_DIRTY_GFN_SIZE: usize = 16;

pub fn ring_bytes(entries: u32) -> usize {
    entries as usize * RAW_DIRTY_GFN_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirty(slot: u32, offset: u64) -> RawDirtyGfn {
        RawDirtyGfn { flags: DIRTY_BIT, slot, offset }
    }

    fn clean() -> RawDirtyGfn {
        RawDirtyGfn { flags: 0, slot: 0, offset: 0 }
    }

    /// specs/baud-snapshot.md §5's `reset_cost_scales_with_write_set` in miniature: harvesting a
    /// ring with a handful of dirty entries followed by untouched (clean) slots returns exactly
    /// the dirty subset, never the ring's full capacity.
    #[test]
    fn harvest_returns_only_the_dirty_prefix_not_full_ring_capacity() {
        let mut ring = vec![dirty(0, 0x1000), dirty(0, 0x2000), dirty(1, 0x3000)];
        ring.extend((0..97).map(|_| clean())); // pad to a 100-slot ring, 97 untouched
        let mut cursor = 0;
        let harvested = harvest(&mut ring, &mut cursor);
        assert_eq!(harvested, vec![(0, 0x1000), (0, 0x2000), (1, 0x3000)]);
        assert_eq!(cursor, 3, "cursor advances exactly past the harvested entries, not the whole ring");
    }

    #[test]
    fn harvest_marks_reset_bit_on_every_harvested_entry_and_leaves_clean_entries_untouched() {
        let mut ring = vec![dirty(0, 0), dirty(1, 0), clean(), clean()];
        let mut cursor = 0;
        harvest(&mut ring, &mut cursor);
        assert_eq!(ring[0].flags, DIRTY_BIT | RESET_BIT);
        assert_eq!(ring[1].flags, DIRTY_BIT | RESET_BIT);
        assert_eq!(ring[2].flags, 0, "an entry never dirtied must never gain RESET");
        assert_eq!(ring[3].flags, 0);
    }

    #[test]
    fn harvest_called_again_with_no_new_kernel_writes_harvests_nothing() {
        let mut ring = vec![dirty(0, 0), dirty(1, 0), clean()];
        let mut cursor = 0;
        let first = harvest(&mut ring, &mut cursor);
        assert_eq!(first.len(), 2);
        let second = harvest(&mut ring, &mut cursor);
        assert!(second.is_empty(), "no new DIRTY entries since the last harvest -> nothing new to report");
    }

    #[test]
    fn harvest_wraps_the_cursor_around_the_ring_end() {
        // cursor starts at the last slot; two dirty entries live at indices 3 (last) and 0 (wrap).
        let mut ring = vec![dirty(9, 0), clean(), clean(), dirty(8, 0)];
        let mut cursor = 3;
        let harvested = harvest(&mut ring, &mut cursor);
        assert_eq!(harvested, vec![(8, 0), (9, 0)], "must wrap past the ring end back to index 0");
        assert_eq!(cursor, 1, "wrapped past the two dirty entries (3 -> 0 -> 1), stopping at the clean slot");
    }

    #[test]
    fn harvest_on_an_empty_ring_is_a_noop() {
        let mut ring: Vec<RawDirtyGfn> = Vec::new();
        let mut cursor = 0;
        assert!(harvest(&mut ring, &mut cursor).is_empty());
    }

    #[test]
    fn harvest_stops_immediately_when_the_cursor_slot_is_already_clean() {
        let mut ring = vec![clean(), dirty(0, 0)];
        let mut cursor = 0;
        assert!(harvest(&mut ring, &mut cursor).is_empty());
        assert_eq!(cursor, 0, "cursor must not advance past an entry it did not harvest");
    }

    #[test]
    fn ring_bytes_matches_entry_count_times_kvm_dirty_gfn_size() {
        assert_eq!(ring_bytes(0), 0);
        assert_eq!(ring_bytes(1), 16);
        assert_eq!(ring_bytes(4096), 4096 * 16);
    }

    proptest::proptest! {
        /// specs/baud-snapshot.md §5, general form: for *any* prefix of dirty entries followed by
        /// clean ones, the harvested count equals exactly the dirty prefix length — never the full
        /// ring, regardless of ring size or how many pages were actually touched.
        #[test]
        fn harvest_count_always_equals_the_dirty_prefix_length(total in 1usize..200, dirty_count in 0usize..200) {
            let dirty_count = dirty_count.min(total);
            let mut ring: Vec<RawDirtyGfn> = (0..total)
                .map(|i| if i < dirty_count { dirty(i as u32, i as u64) } else { clean() })
                .collect();
            let mut cursor = 0;
            let harvested = harvest(&mut ring, &mut cursor);
            proptest::prop_assert_eq!(harvested.len(), dirty_count);
            proptest::prop_assert_eq!(cursor, dirty_count % total);
        }
    }
}
