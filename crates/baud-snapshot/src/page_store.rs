// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Content-addressed guest-RAM pages (specs/baud-snapshot.md §3's capture-set table: "Guest RAM |
// read each memslot backing", and §4's "Per-branch memory ∝ the child's write set, not total RAM"
// — that guarantee starts here: two universes whose page N has identical bytes share the exact
// same `Arc`, so capturing a second universe right after the first costs nothing for the pages
// neither touched, hardware-independent of any real userfaultfd wiring). blake3-hashed (already a
// workspace dependency, and the same hash `linux::Multiverse::ram_hash` in `baud-multiverse` uses
// for `double_boot_memory_identical` — one hash algorithm across the whole determinism story, not
// two).

use std::collections::HashMap;
use std::sync::Arc;

/// One guest-RAM page. 4 KiB matches the identity map's leaf granularity conceptually (the boot
/// flow's own page tables use 2 MiB leaves for the *paging* structures, `layout.rs`, but the
/// snapshot/dirty-tracking granularity KVM exposes — memslot dirty bitmaps, `KVM_CAP_DIRTY_LOG_RING`
/// entries — is always base 4 KiB pages regardless of the guest's own paging leaf size).
pub const PAGE_SIZE: usize = 4096;

/// blake3 of one page's bytes. `Eq`/`Hash` so it is usable as a `HashMap` key directly (no re-hash
/// of an already-cryptographic digest).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageHash([u8; 32]);

impl PageHash {
    pub fn of(bytes: &[u8; PAGE_SIZE]) -> Self {
        PageHash(*blake3::hash(bytes).as_bytes())
    }

    pub fn to_hex(self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }
}

/// A shared handle to one page's content. Cloning is cheap (`Arc::clone`) and two `PageRef`s
/// produced by [`PageStore::intern`]ing identical bytes are the *same* allocation
/// (`Arc::ptr_eq`) — that identity, not just content equality, is what makes branching cheap:
/// a branch's unwritten pages are literally the parent's `Arc`s, not copies.
#[derive(Clone, Debug)]
pub struct PageRef {
    hash: PageHash,
    bytes: Arc<[u8; PAGE_SIZE]>,
}

impl PageRef {
    pub fn hash(&self) -> PageHash {
        self.hash
    }

    pub fn bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.bytes
    }

    /// Whether `self` and `other` are the exact same backing allocation (not merely
    /// content-equal) — the property [`PageStore::intern`] guarantees for identical content and
    /// that `universe::dirty_pages` relies on as a cheap first check before falling back to a full
    /// byte comparison (two different `PageStore`s could in principle produce content-equal but
    /// distinct `Arc`s; this method only tells you about sharing, not content).
    pub fn is_same_allocation(&self, other: &PageRef) -> bool {
        Arc::ptr_eq(&self.bytes, &other.bytes)
    }
}

impl PartialEq for PageRef {
    /// Content equality (by hash, collision-negligible at blake3's strength) — *not* allocation
    /// identity. Two pages read from different `PageStore`s with the same bytes compare equal even
    /// though [`is_same_allocation`](Self::is_same_allocation) would be `false` for them.
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}
impl Eq for PageRef {}

/// Deduplicates guest-RAM pages by content across every universe captured through it. One
/// `PageStore` is meant to live for the lifetime of a whole run's exploration tree (shared by every
/// `Universe` captured along the way), not per-capture — capturing from it repeatedly is what lets
/// unchanged pages cost nothing.
#[derive(Default)]
pub struct PageStore {
    pages: HashMap<PageHash, Arc<[u8; PAGE_SIZE]>>,
}

impl PageStore {
    pub fn new() -> Self {
        PageStore { pages: HashMap::new() }
    }

    /// Intern one page's bytes, returning a [`PageRef`] shared with every other page in this store
    /// that has identical content. The number of *distinct* allocations this store holds
    /// ([`len`](Self::len)) never exceeds the number of distinct page contents ever interned,
    /// regardless of how many universes/branches reference them.
    pub fn intern(&mut self, bytes: &[u8; PAGE_SIZE]) -> PageRef {
        let hash = PageHash::of(bytes);
        let arc = self.pages.entry(hash).or_insert_with(|| Arc::new(*bytes)).clone();
        PageRef { hash, bytes: arc }
    }

    /// How many distinct page contents this store currently holds — the content-addressed
    /// deduplication guarantee's direct observable (specs/baud-snapshot.md §4).
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(fill: u8) -> [u8; PAGE_SIZE] {
        [fill; PAGE_SIZE]
    }

    #[test]
    fn interning_identical_content_shares_the_same_allocation() {
        let mut store = PageStore::new();
        let a = store.intern(&page(0x42));
        let b = store.intern(&page(0x42));
        assert!(a.is_same_allocation(&b), "identical content must share one Arc, not be copied");
        assert_eq!(store.len(), 1, "one distinct content -> one stored allocation");
    }

    #[test]
    fn interning_different_content_grows_the_store() {
        let mut store = PageStore::new();
        store.intern(&page(0x00));
        store.intern(&page(0x01));
        store.intern(&page(0x00)); // repeat — must not grow further
        assert_eq!(store.len(), 2);
    }

    /// Directly exercises specs/baud-snapshot.md §4's "per-branch memory ∝ write set" claim at the
    /// page-store level: capturing 1,000 "universes" that are all-zero except one differing page
    /// each costs `1 (shared zero page) + 1,000 (each universe's one unique page)` distinct
    /// allocations, not `1,000 * pages_per_universe`.
    #[test]
    fn a_thousand_mostly_identical_universes_share_the_common_pages() {
        let mut store = PageStore::new();
        const PAGES_PER_UNIVERSE: usize = 16;
        for i in 0..1000u32 {
            let mut refs = Vec::with_capacity(PAGES_PER_UNIVERSE);
            for p in 0..PAGES_PER_UNIVERSE {
                if p == 0 {
                    // the one page this universe's branch actually wrote — unique content
                    let mut bytes = page(0);
                    bytes[..4].copy_from_slice(&(i + 1).to_le_bytes()); // +1: never collide with the all-zero page
                    refs.push(store.intern(&bytes));
                } else {
                    refs.push(store.intern(&page(0))); // shared, unwritten zero page
                }
            }
            assert!(refs[1].is_same_allocation(&refs[2]), "unwritten pages within one universe share too");
        }
        assert_eq!(store.len(), 1 + 1000, "1 shared zero page + 1000 unique pages, not 1000*16");
    }

    #[test]
    fn page_hash_hex_is_stable_for_identical_content() {
        assert_eq!(PageHash::of(&page(7)).to_hex(), PageHash::of(&page(7)).to_hex());
        assert_ne!(PageHash::of(&page(7)).to_hex(), PageHash::of(&page(8)).to_hex());
    }
}
