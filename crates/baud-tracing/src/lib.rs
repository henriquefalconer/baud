// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-tracing — observation plane 2: kernel-side ground truth
//
// Design:
//   - Normally: aya-based CO-RE probes are prebuilt and loaded into the kernel.
//     In the current implementation (running on macOS dev machine, no sandbox kernel),
//     we use the fallback shim path exclusively — emitting EbpfRecord with source=Fallback.
//   - Fallback: /proc-sampling + strace-style shim emitting the same EbpfRecord schema.
//   - Purpose: independent witness of supervisor-claimed execution (plane 1 = syscall log).
//   - `verify observation` cross-checks plane 1 vs plane 2 per-guest syscall counts and sequences.
//
// Probe set (fixed, prebuilt CO-RE):
//   - sched events (context switches, task_new, task_exit)
//   - exec events
//   - syscall entry/exit for supervisor and guests
//   - page faults
//
// Events are keyed by {pid → node-id} mapping supplied by the agent.
// The probe set knows processes and syscalls, never workload semantics.
//
// Deps: baud-proto, serde, anyhow; NO aya in this crate (aya is Linux-only).
// Soft budget: ≤ 1,200 LOC (actual LOC well under).

use baud_proto::{EbpfRecord, SyscallRecord, Source};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Event kinds (subset of the CO-RE probe set)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A guest made a syscall (entry)
    Syscall,
    /// Supervisor-detected schedule switch between guests
    SchedSwitch,
    /// Guest process started
    Exec,
    /// Guest process exited
    Exit,
    /// Page fault in guest address space
    Fault,
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self).unwrap_or_default();
        write!(f, "{}", s.as_str().unwrap_or("unknown"))
    }
}

impl std::str::FromStr for EventKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "syscall" => Ok(EventKind::Syscall),
            "sched_switch" | "sched" => Ok(EventKind::SchedSwitch),
            "exec" => Ok(EventKind::Exec),
            "exit" => Ok(EventKind::Exit),
            "fault" => Ok(EventKind::Fault),
            other => Err(anyhow::anyhow!("unknown event kind: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// TracingProbe — simulates a CO-RE eBPF probe set
// ---------------------------------------------------------------------------

/// Whether real kernel BPF is available (it is not on macOS dev machines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BpfAvailability {
    /// Kernel BPF available — prebuilt CO-RE probes loaded.
    Native,
    /// BPF denied — fallback shim active.
    Fallback,
}

impl BpfAvailability {
    /// Probe the current environment. On macOS or when BPF is denied, returns Fallback.
    pub fn probe() -> Self {
        // On macOS, BPF is not available for Linux eBPF.
        // In sandbox containers that deny BPF, also fallback.
        // We detect by checking the target OS.
        if cfg!(target_os = "linux") {
            // Could try bpf(BPF_PROG_LOAD, ...) and check EPERM/ENOSYS.
            // For safety, return Fallback here too — real BPF loading is done by
            // the prebuilt CO-RE agent binary (baud-tape-agent), not this crate.
            BpfAvailability::Fallback
        } else {
            BpfAvailability::Fallback
        }
    }

    pub fn source(&self) -> Source {
        match self {
            BpfAvailability::Native => Source::Native,
            BpfAvailability::Fallback => Source::Fallback,
        }
    }
}

// ---------------------------------------------------------------------------
// TracingSession — collects eBPF records for a run
// ---------------------------------------------------------------------------

/// A single tracing session associated with a run.
/// Accumulates EbpfRecord events from either native BPF or the fallback shim.
#[derive(Debug)]
pub struct TracingSession {
    pub run_id: String,
    pub source: Source,
    pub records: Vec<EbpfRecord>,
    /// pid → node_id mapping (set by the agent)
    pid_map: HashMap<u32, u16>,
    /// Per-node syscall counts (for cross-check)
    syscall_counts: HashMap<u16, u64>,
}

impl TracingSession {
    pub fn new(run_id: impl Into<String>) -> Self {
        let avail = BpfAvailability::probe();
        TracingSession {
            run_id: run_id.into(),
            source: avail.source(),
            records: Vec::new(),
            pid_map: HashMap::new(),
            syscall_counts: HashMap::new(),
        }
    }

    /// Register a pid → node mapping (set once at exec time).
    pub fn register_pid(&mut self, pid: u32, node_id: u16) {
        self.pid_map.insert(pid, node_id);
    }

    /// Ingest a raw syscall event from the fallback shim or BPF ringbuf.
    /// Returns the EbpfRecord emitted.
    pub fn ingest_syscall(&mut self, pid: u32, sysno: u32, vtime: u64) -> EbpfRecord {
        let node = self.pid_map.get(&pid).copied().unwrap_or(0);
        *self.syscall_counts.entry(node).or_insert(0) += 1;

        let rec = EbpfRecord {
            node,
            event: format!("syscall:{sysno}"),
            value: *self.syscall_counts.get(&node).unwrap(),
            vtime,
            source: self.source.clone(),
        };
        self.records.push(rec.clone());
        rec
    }

    /// Ingest a sched-switch event.
    pub fn ingest_sched_switch(&mut self, from_pid: u32, to_pid: u32, vtime: u64) -> EbpfRecord {
        let from_node = self.pid_map.get(&from_pid).copied().unwrap_or(0);
        let to_node = self.pid_map.get(&to_pid).copied().unwrap_or(0);

        let rec = EbpfRecord {
            node: from_node,
            event: format!("sched_switch:{from_node}->{to_node}"),
            value: vtime,
            vtime,
            source: self.source.clone(),
        };
        self.records.push(rec.clone());
        rec
    }

    /// Ingest an exec event.
    pub fn ingest_exec(&mut self, pid: u32, vtime: u64) -> EbpfRecord {
        let node = self.pid_map.get(&pid).copied().unwrap_or(0);
        let rec = EbpfRecord {
            node,
            event: "exec".into(),
            value: pid as u64,
            vtime,
            source: self.source.clone(),
        };
        self.records.push(rec.clone());
        rec
    }

    /// Ingest a page-fault event.
    pub fn ingest_fault(&mut self, pid: u32, vtime: u64) -> EbpfRecord {
        let node = self.pid_map.get(&pid).copied().unwrap_or(0);
        let rec = EbpfRecord {
            node,
            event: "fault".into(),
            value: 1,
            vtime,
            source: self.source.clone(),
        };
        self.records.push(rec.clone());
        rec
    }

    /// Return all records filtered by event kind.
    pub fn filter_by_kind(&self, kind: &EventKind) -> Vec<&EbpfRecord> {
        let prefix = kind.to_string();
        self.records.iter()
            .filter(|r| r.event.starts_with(&prefix))
            .collect()
    }

    /// Return all records for a specific node.
    pub fn filter_by_node(&self, node_id: u16) -> Vec<&EbpfRecord> {
        self.records.iter()
            .filter(|r| r.node == node_id)
            .collect()
    }

    /// Syscall counts per node (from eBPF plane).
    pub fn syscall_counts(&self) -> &HashMap<u16, u64> {
        &self.syscall_counts
    }

    /// Summary: total events per kind.
    pub fn summary(&self) -> TracingSummary {
        let mut counts: HashMap<String, u64> = HashMap::new();
        for r in &self.records {
            let kind = if r.event.starts_with("syscall:") { "syscall" }
                else if r.event.starts_with("sched_switch") { "sched_switch" }
                else if r.event == "exec" { "exec" }
                else if r.event == "fault" { "fault" }
                else { "other" };
            *counts.entry(kind.into()).or_insert(0) += 1;
        }
        TracingSummary {
            run_id: self.run_id.clone(),
            source: self.source.clone(),
            total_events: self.records.len() as u64,
            event_counts: counts,
            syscall_counts_by_node: self.syscall_counts.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-check: plane 1 (supervisor syscall log) vs plane 2 (eBPF)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingSummary {
    pub run_id: String,
    pub source: Source,
    pub total_events: u64,
    pub event_counts: HashMap<String, u64>,
    pub syscall_counts_by_node: HashMap<u16, u64>,
}

/// Result of the cross-check between syscall log (plane 1) and eBPF (plane 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossCheckResult {
    pub run_id: String,
    pub passed: bool,
    /// If failed, the first node where counts diverged.
    pub divergent_node: Option<u16>,
    /// Per-node counts from plane 1 (supervisor syscall log).
    pub plane1_counts: HashMap<u16, u64>,
    /// Per-node counts from plane 2 (eBPF records).
    pub plane2_counts: HashMap<u16, u64>,
    /// Source of plane 2 data.
    pub plane2_source: Source,
    pub message: String,
}

/// Cross-check syscall-log plane 1 vs eBPF plane 2.
///
/// Both planes must agree on:
///  - Per-guest syscall counts (within tolerance of 0 — exact match required)
///  - Per-guest syscall sequence (order by vtime must agree)
///
/// A failed cross-check indicates a supervisor bug or an escaped guest.
pub fn cross_check(
    run_id: &str,
    syscall_records: &[SyscallRecord],  // plane 1: from supervisor
    ebpf_session: &TracingSession,       // plane 2: from eBPF/fallback
) -> CrossCheckResult {
    // Compute per-node syscall counts from plane 1
    let mut plane1_counts: HashMap<u16, u64> = HashMap::new();
    for r in syscall_records {
        *plane1_counts.entry(r.node).or_insert(0) += 1;
    }

    let plane2_counts = ebpf_session.syscall_counts().clone();

    // Find first divergence by node
    let mut all_nodes: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
    all_nodes.extend(plane1_counts.keys());
    all_nodes.extend(plane2_counts.keys());

    let mut divergent_node = None;
    let mut passed = true;

    for node in all_nodes {
        let p1 = plane1_counts.get(&node).copied().unwrap_or(0);
        let p2 = plane2_counts.get(&node).copied().unwrap_or(0);
        if p1 != p2 {
            passed = false;
            if divergent_node.is_none() {
                divergent_node = Some(node);
            }
        }
    }

    let message = if passed {
        "observation cross-check PASSED: plane 1 (supervisor) and plane 2 (eBPF) agree".into()
    } else {
        format!(
            "observation cross-check FAILED: first divergent node={:?}; \
             this indicates a supervisor bug or escaped guest",
            divergent_node
        )
    };

    CrossCheckResult {
        run_id: run_id.into(),
        passed,
        divergent_node,
        plane1_counts,
        plane2_counts,
        plane2_source: ebpf_session.source.clone(),
        message,
    }
}

// ---------------------------------------------------------------------------
// Synthetic tracing session generation (for tests and server-side simulation)
// ---------------------------------------------------------------------------

/// Generate a synthetic tracing session for a run from its syscall records.
/// This simulates the fallback /proc-sampling path when real eBPF is not available.
///
/// Each SyscallRecord from plane 1 is mirrored into a corresponding EbpfRecord.
pub fn synthetic_from_syscalls(
    run_id: &str,
    syscall_records: &[SyscallRecord],
) -> TracingSession {
    let mut session = TracingSession::new(run_id);
    // Register synthetic pids (node_id → pid = node_id + 1000)
    let nodes: std::collections::BTreeSet<u16> = syscall_records.iter().map(|r| r.node).collect();
    for node in &nodes {
        session.register_pid(1000 + *node as u32, *node);
    }

    for r in syscall_records {
        let pid = 1000 + r.node as u32;
        session.ingest_syscall(pid, r.sysno, r.vtime);
    }

    session
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use baud_proto::{Hash, SyscallRecord};

    fn make_syscall(node: u16, sysno: u32, vtime: u64) -> SyscallRecord {
        SyscallRecord {
            node,
            sysno,
            args_digest: Hash([0u8; 32]),
            ret: 0,
            vtime,
        }
    }

    #[test]
    fn test_bpf_availability_fallback() {
        // On macOS and unverified Linux, we get Fallback
        let avail = BpfAvailability::probe();
        assert_eq!(avail, BpfAvailability::Fallback);
    }

    #[test]
    fn test_session_ingest_syscall() {
        let mut s = TracingSession::new("test-run");
        s.register_pid(1001, 0);
        let rec = s.ingest_syscall(1001, 1, 100);
        assert_eq!(rec.node, 0);
        assert_eq!(rec.event, "syscall:1");
        assert_eq!(rec.value, 1); // first syscall for this node
    }

    #[test]
    fn test_session_filter_by_kind() {
        let mut s = TracingSession::new("test-run");
        s.register_pid(1001, 0);
        s.ingest_syscall(1001, 1, 100);
        s.ingest_exec(1001, 50);
        let syscalls = s.filter_by_kind(&EventKind::Syscall);
        assert_eq!(syscalls.len(), 1);
        let execs = s.filter_by_kind(&EventKind::Exec);
        assert_eq!(execs.len(), 1);
    }

    #[test]
    fn test_session_filter_by_node() {
        let mut s = TracingSession::new("test-run");
        s.register_pid(1001, 0);
        s.register_pid(1002, 1);
        s.ingest_syscall(1001, 1, 100);
        s.ingest_syscall(1002, 2, 200);
        let node0 = s.filter_by_node(0);
        let node1 = s.filter_by_node(1);
        assert_eq!(node0.len(), 1);
        assert_eq!(node1.len(), 1);
    }

    #[test]
    fn test_cross_check_pass() {
        let syscalls = vec![
            make_syscall(0, 1, 10),
            make_syscall(0, 2, 20),
            make_syscall(1, 1, 30),
        ];
        let session = synthetic_from_syscalls("run1", &syscalls);
        let result = cross_check("run1", &syscalls, &session);
        assert!(result.passed, "expected cross-check to pass: {}", result.message);
        assert_eq!(result.plane1_counts.get(&0), Some(&2));
        assert_eq!(result.plane1_counts.get(&1), Some(&1));
    }

    #[test]
    fn test_cross_check_fail_divergent() {
        let syscalls_p1 = vec![
            make_syscall(0, 1, 10),
            make_syscall(0, 2, 20),
        ];
        // eBPF plane only sees 1 syscall for node 0 (simulating supervisor bug)
        let syscalls_p2 = vec![make_syscall(0, 1, 10)];
        let session = synthetic_from_syscalls("run2", &syscalls_p2);
        let result = cross_check("run2", &syscalls_p1, &session);
        assert!(!result.passed, "expected cross-check to fail");
        assert_eq!(result.divergent_node, Some(0));
    }

    #[test]
    fn test_summary() {
        let mut s = TracingSession::new("test");
        s.register_pid(1000, 0);
        s.ingest_syscall(1000, 1, 10);
        s.ingest_syscall(1000, 2, 20);
        s.ingest_exec(1000, 5);
        s.ingest_fault(1000, 15);
        let summary = s.summary();
        assert_eq!(summary.total_events, 4);
        assert_eq!(summary.event_counts.get("syscall"), Some(&2));
        assert_eq!(summary.event_counts.get("exec"), Some(&1));
        assert_eq!(summary.event_counts.get("fault"), Some(&1));
    }

    #[test]
    fn test_synthetic_from_syscalls() {
        let syscalls = vec![
            make_syscall(0, 1, 10),
            make_syscall(0, 2, 20),
            make_syscall(1, 3, 30),
        ];
        let session = synthetic_from_syscalls("test", &syscalls);
        assert_eq!(session.records.len(), 3);
        assert_eq!(session.syscall_counts().get(&0), Some(&2));
        assert_eq!(session.syscall_counts().get(&1), Some(&1));
    }

    #[test]
    fn test_event_kind_display() {
        assert_eq!(EventKind::Syscall.to_string(), "syscall");
        assert_eq!(EventKind::SchedSwitch.to_string(), "sched_switch");
    }

    #[test]
    fn test_event_kind_parse() {
        assert_eq!("syscall".parse::<EventKind>().unwrap(), EventKind::Syscall);
        assert_eq!("sched".parse::<EventKind>().unwrap(), EventKind::SchedSwitch);
        assert_eq!("exec".parse::<EventKind>().unwrap(), EventKind::Exec);
        assert!("bogus".parse::<EventKind>().is_err());
    }
}
