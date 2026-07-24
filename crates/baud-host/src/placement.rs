// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Fleet placement: one physical core per VM, never splitting hyperthread siblings
// (specs/baud-host.md §5).

use serde::{Deserialize, Serialize};

/// One physical core and the logical CPU ids (SMT siblings) it comprises. `sibling_threads` has
/// one entry when SMT is disabled (or absent) and two when hyperthreading pairs share this core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreTopology {
    pub physical_id: usize,
    pub sibling_threads: Vec<usize>,
}

/// The host's core layout, as gathered alongside a [`crate::Probe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    pub cores: Vec<CoreTopology>,
    /// Physical cores held back for host bookkeeping / RCU / IRQ handling (2-4 per socket per
    /// specs/baud-host.md §5); never assigned to a VM.
    pub housekeeping_reserved: usize,
}

/// A refused placement request: `n` VMs do not fit this host's capacity.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("requested {requested} VM(s), host capacity is {capacity} (never oversubscribes or splits SMT siblings)")]
pub struct PlacementError {
    pub requested: usize,
    pub capacity: usize,
}

/// An accepted placement: each VM gets one whole physical core (including both SMT siblings, so
/// no other VM can ever land on the paired thread).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub assigned_cores: Vec<CoreTopology>,
}

impl Placement {
    /// True when no two assigned cores share a logical CPU (i.e. no VM was split across, or
    /// double-booked onto, an SMT sibling pair). Always true for placements produced by
    /// [`place`], since it assigns whole physical cores — this is the property the test asserts.
    pub fn no_two_on_sibling_threads(&self) -> bool {
        let mut seen = std::collections::HashSet::new();
        for core in &self.assigned_cores {
            for &thread in &core.sibling_threads {
                if !seen.insert(thread) {
                    return false;
                }
            }
        }
        true
    }
}

/// One physical core per VM, taken from the non-housekeeping cores, in physical-id order.
/// Refuses outright — never partially places, never splits a sibling pair — when `n` exceeds
/// capacity.
pub fn place(topology: &Topology, n: usize) -> Result<Placement, PlacementError> {
    let capacity = topology.cores.len().saturating_sub(topology.housekeeping_reserved);
    if n > capacity {
        return Err(PlacementError { requested: n, capacity });
    }
    let assigned_cores = topology
        .cores
        .iter()
        .take(n)
        .cloned()
        .collect();
    Ok(Placement { assigned_cores })
}
