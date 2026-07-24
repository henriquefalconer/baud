// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// In-memory branch-point bookkeeping (specs/baud-snapshot.md §2's architecture diagram: "the
// branch tree" is part of this crate; §5's "The tree: snapshot at each branch point; exploration
// forks from the nearest one instead of replaying the prefix" — todo.md §5, problem #22
// "Shrinking re-runs from zero (slow)" -> "Fork from nearest snapshot"). This is bookkeeping only
// — it tracks *where* branch points sit along each lineage's tape offset, not the `Universe`
// payload itself (that is `Universe`/`PageStore`, kept separately by the caller and typically
// content-addressed onward into `baud-snapshot-store` for durability, which this crate does not
// own per its own architecture diagram).

/// Identifies one node (a branch point) in the tree. Opaque — callers never construct one by hand,
/// only receive them from [`Tree::insert`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

struct Node {
    parent: Option<NodeId>,
    /// How many tape bytes had been consumed at this branch point — the coordinate
    /// [`Tree::nearest_ancestor_at_or_before`] searches on.
    tape_offset: usize,
}

/// A forest of branch points, each an ancestor chain back to some root (specs/baud-snapshot.md
/// §5's "tree, not line"). Roots are nodes inserted with `parent: None` — a run may have more than
/// one root (e.g. a fresh boot alongside a restored-from-store universe), so this is technically a
/// forest, kept under one `Tree` for a single lookup surface.
#[derive(Default)]
pub struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    pub fn new() -> Self {
        Tree { nodes: Vec::new() }
    }

    /// Record a new branch point. `parent` is the node this one forked from (`None` for a fresh
    /// root, e.g. the very first boot); `tape_offset` is how many tape bytes had been consumed
    /// when the snapshot backing this node was taken.
    pub fn insert(&mut self, parent: Option<NodeId>, tape_offset: usize) -> NodeId {
        self.nodes.push(Node { parent, tape_offset });
        NodeId(self.nodes.len() - 1)
    }

    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.nodes[node.0].parent
    }

    pub fn tape_offset(&self, node: NodeId) -> usize {
        self.nodes[node.0].tape_offset
    }

    /// The ancestor chain from `node` up to its root, `node` first.
    pub fn path_to_root(&self, node: NodeId) -> Vec<NodeId> {
        let mut path = vec![node];
        let mut current = node;
        while let Some(parent) = self.parent(current) {
            path.push(parent);
            current = parent;
        }
        path
    }

    /// The deepest ancestor of `node` (`node` itself included) whose `tape_offset` is `<=
    /// target_offset` — the node exploration/shrinking should fork the new continuation from,
    /// instead of replaying from the root (specs/baud-snapshot.md §5's
    /// `shrink_reproduces_from_nearest_snapshot`: "shrinking a finding forks from the nearest
    /// snapshot (not from boot)"). Returns `None` only if every ancestor's offset already exceeds
    /// `target_offset` (nothing in this lineage is early enough to fork from).
    pub fn nearest_ancestor_at_or_before(&self, node: NodeId, target_offset: usize) -> Option<NodeId> {
        self.path_to_root(node)
            .into_iter()
            .filter(|&n| self.tape_offset(n) <= target_offset)
            .max_by_key(|&n| self.tape_offset(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_to_root_walks_every_ancestor_in_order() {
        let mut tree = Tree::new();
        let root = tree.insert(None, 0);
        let mid = tree.insert(Some(root), 100);
        let leaf = tree.insert(Some(mid), 250);

        assert_eq!(tree.path_to_root(leaf), vec![leaf, mid, root]);
        assert_eq!(tree.path_to_root(root), vec![root]);
    }

    /// The core `shrink_reproduces_from_nearest_snapshot` guarantee at the bookkeeping level: the
    /// nearest ancestor at-or-before a target offset must be the *deepest* one, not the root, and
    /// not one that overshoots the target.
    #[test]
    fn nearest_ancestor_picks_the_deepest_node_not_past_the_target() {
        let mut tree = Tree::new();
        let root = tree.insert(None, 0);
        let a = tree.insert(Some(root), 100);
        let b = tree.insert(Some(a), 300);
        let c = tree.insert(Some(b), 900);

        assert_eq!(tree.nearest_ancestor_at_or_before(c, 500), Some(b), "must not overshoot 500 by picking c(900)");
        assert_eq!(tree.nearest_ancestor_at_or_before(c, 900), Some(c), "an exact match is itself the nearest");
        assert_eq!(tree.nearest_ancestor_at_or_before(c, 50), Some(root), "falls back to root if nothing else qualifies");
        assert_eq!(tree.nearest_ancestor_at_or_before(c, 1_000_000), Some(c), "deepest node still wins when target is far beyond it");
    }

    #[test]
    fn nearest_ancestor_returns_none_when_even_the_root_is_past_the_target() {
        let mut tree = Tree::new();
        let root = tree.insert(None, 500);
        assert_eq!(tree.nearest_ancestor_at_or_before(root, 10), None);
    }

    #[test]
    fn multiple_roots_form_an_independent_forest() {
        let mut tree = Tree::new();
        let root_a = tree.insert(None, 0);
        let root_b = tree.insert(None, 0);
        let leaf_a = tree.insert(Some(root_a), 50);
        assert_eq!(tree.path_to_root(leaf_a), vec![leaf_a, root_a]);
        assert_ne!(root_a, root_b, "distinct roots must have distinct ids");
    }
}
