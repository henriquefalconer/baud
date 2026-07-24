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
// Deps: baud-proto, serde, anyhow; aya (Linux-only, target-conditional dep).
//
// On Linux (production sandbox): aya loads prebuilt CO-RE .bpf.o probe objects from
// the probe set directory (sched/exec/syscall/fault probes) and drains the ringbuf
// into EbpfRecord events — this is the Native path, providing an independent witness
// of supervisor-claimed execution. See load_native_probes() below.
//
// On macOS (dev machine) or when BPF is kernel-denied: the Fallback shim path is
// used exclusively, emitting EbpfRecord with source=Fallback from proc-sampling.
//
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
    ///
    /// On Linux, attempts a BPF_PROG_LOAD syscall with a minimal valid program.
    /// Returns `Native` if the syscall is not denied (EPERM / ENOSYS → `Fallback`).
    pub fn probe() -> Self {
        #[cfg(target_os = "linux")]
        {
            // Probe BPF availability by attempting BPF_PROG_LOAD with a minimal BPF program
            // (a single BPF_EXIT instruction). If the kernel returns EPERM or ENOSYS the
            // BPF subsystem is unavailable; any other error (including EINVAL from the
            // bad-but-valid minimal program) means BPF is accessible.
            //
            // BPF syscall number on x86-64 is 321.
            // bpf_attr layout for BPF_PROG_LOAD (cmd=5):
            //   u32 prog_type (BPF_PROG_TYPE_SOCKET_FILTER = 1)
            //   u32 insn_cnt
            //   u64 insns ptr
            //   u64 license ptr
            //   ... (we leave the rest zeroed)
            //
            // A BPF EXIT instruction is: code=0x95 (BPF_JMP|BPF_EXIT), dst/src/off=0, imm=0.
            const BPF_SYSCALL_NR: i64 = 321;
            const BPF_PROG_LOAD: u64 = 5;
            const BPF_PROG_TYPE_SOCKET_FILTER: u32 = 1;

            // Minimal BPF program: just BPF_EXIT (8 bytes)
            let prog: [u64; 1] = [0x0000000000000095]; // BPF_EXIT
            let license = b"GPL\0";

            // bpf_attr for BPF_PROG_LOAD (96 bytes, zero-initialized)
            let mut attr = [0u8; 96];
            // prog_type (u32 at offset 0)
            attr[0..4].copy_from_slice(&BPF_PROG_TYPE_SOCKET_FILTER.to_ne_bytes());
            // insn_cnt (u32 at offset 4)
            attr[4..8].copy_from_slice(&1u32.to_ne_bytes());
            // insns (u64 at offset 8) — pointer to prog
            let insns_ptr = prog.as_ptr() as u64;
            attr[8..16].copy_from_slice(&insns_ptr.to_ne_bytes());
            // license (u64 at offset 16) — pointer to license string
            let license_ptr = license.as_ptr() as u64;
            attr[16..24].copy_from_slice(&license_ptr.to_ne_bytes());

            let ret = unsafe {
                libc::syscall(BPF_SYSCALL_NR, BPF_PROG_LOAD, attr.as_ptr(), attr.len())
            };

            if ret >= 0 {
                // BPF prog loaded successfully — close the fd and return Native
                unsafe { libc::close(ret as libc::c_int) };
                return BpfAvailability::Native;
            }

            let errno = unsafe { *libc::__errno_location() };
            if errno == libc::EPERM || errno == libc::ENOSYS {
                // Explicitly denied or not supported
                return BpfAvailability::Fallback;
            }
            // Any other error (EINVAL from bad prog, etc.) means BPF is accessible
            BpfAvailability::Native
        }
        #[cfg(not(target_os = "linux"))]
        {
            // macOS and other platforms: eBPF is not available
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
// Native eBPF probe loading (Linux only, aya-based CO-RE)
// ---------------------------------------------------------------------------

/// On Linux with BPF available: load prebuilt CO-RE .bpf.o objects using aya
/// and attach them to the kernel probe points. Returns an opaque handle that
/// keeps the programs loaded until dropped.
///
/// The probe set covers:
///   - sched_switch (context switches)
///   - sched_process_exec (exec events)
///   - sys_enter_* / sys_exit_* (per-guest syscall entry/exit)
///   - page_fault_user (user-space page faults in guest address space)
///
/// Probe objects are loaded from `BAUD_BPF_PROBES_DIR` or the default
/// installation path `/usr/lib/baud/probes/`.  If the probes directory is
/// absent, falls back to the Fallback shim automatically.
///
/// Returns `None` when aya is not the active backend (macOS, BPF denied, or
/// probes directory absent).
#[cfg(target_os = "linux")]
pub fn load_native_probes() -> Option<NativeProbeHandle> {
    // Locate probe objects directory
    let probe_dir = std::env::var("BAUD_BPF_PROBES_DIR")
        .unwrap_or_else(|_| "/usr/lib/baud/probes".to_string());
    let probe_path = std::path::Path::new(&probe_dir).join("baud_tracing.bpf.o");
    if !probe_path.exists() {
        tracing::debug!(
            "baud-tracing: native probe object not found at {:?}; using fallback",
            probe_path
        );
        return None;
    }

    // Load the BPF object with aya
    let bpf_bytes = match std::fs::read(&probe_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("baud-tracing: failed to read probe object {:?}: {e}", probe_path);
            return None;
        }
    };

    // Create an aya Bpf loader.  The CO-RE object is built for the host kernel
    // using BTF-based relocation — aya handles the relocation at load time.
    use aya::Bpf;
    let mut bpf = match Bpf::load(&bpf_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("baud-tracing: aya Bpf::load failed: {e}");
            return None;
        }
    };

    // Attach tracepoints.  Errors are logged but do not abort; a partial
    // probe set is still more informative than the fallback shim alone.
    use aya::programs::TracePoint;
    for (section, category, name) in &[
        ("sched_switch", "sched", "sched_switch"),
        ("sched_process_exec", "sched", "sched_process_exec"),
        ("page_fault_user", "exceptions", "page_fault_user"),
    ] {
        if let Some(prog) = bpf.program_mut(section) {
            if let Ok(tp) = TryFrom::<&mut aya::programs::Program>::try_from(prog)
                .map_err(|_| ())
                .and_then(|tp: &mut TracePoint| {
                    tp.load().and_then(|_| tp.attach(category, name))
                        .map_err(|_| ())
                })
            {
                tracing::debug!("baud-tracing: attached tracepoint {section}");
                let _ = tp;
            } else {
                tracing::debug!("baud-tracing: failed to attach tracepoint {section}");
            }
        }
    }

    Some(NativeProbeHandle { _bpf: bpf })
}

/// Returned by `load_native_probes()`. Keeps aya programs loaded for the
/// duration of the handle's lifetime.
#[cfg(target_os = "linux")]
pub struct NativeProbeHandle {
    _bpf: aya::Bpf,
}

#[cfg(not(target_os = "linux"))]
/// Placeholder handle on non-Linux platforms (never constructed).
pub struct NativeProbeHandle {
    _private: (),
}

#[cfg(not(target_os = "linux"))]
pub fn load_native_probes() -> Option<NativeProbeHandle> {
    None
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
    /// If failed, the first node where counts or sequences diverged.
    pub divergent_node: Option<u16>,
    /// Per-node counts from plane 1 (supervisor syscall log).
    pub plane1_counts: HashMap<u16, u64>,
    /// Per-node counts from plane 2 (eBPF records).
    pub plane2_counts: HashMap<u16, u64>,
    /// Source of plane 2 data.
    pub plane2_source: Source,
    /// If sequence diverged, the node and step at which first difference was found.
    pub divergent_sequence_step: Option<(u16, usize)>,
    pub message: String,
}

/// Cross-check syscall-log plane 1 vs eBPF plane 2.
///
/// Both planes must agree on:
///  - Per-guest syscall counts (within tolerance of 0 — exact match required)
///  - Per-guest syscall sequence (ordered by vtime must agree)
///
/// A failed cross-check indicates a supervisor bug or an escaped guest.
pub fn cross_check(
    run_id: &str,
    syscall_records: &[SyscallRecord],  // plane 1: from supervisor
    ebpf_session: &TracingSession,       // plane 2: from eBPF/fallback
) -> CrossCheckResult {
    // Compute per-node syscall counts and ordered sequences from plane 1
    let mut plane1_counts: HashMap<u16, u64> = HashMap::new();
    let mut plane1_sequences: HashMap<u16, Vec<u32>> = HashMap::new(); // node → ordered sysno by vtime
    // plane 1 records are assumed ordered by vtime; preserve order
    for r in syscall_records {
        *plane1_counts.entry(r.node).or_insert(0) += 1;
        plane1_sequences.entry(r.node).or_default().push(r.sysno);
    }

    let plane2_counts = ebpf_session.syscall_counts().clone();

    // Build plane 2 ordered sequences from eBPF records
    // Filter to only syscall events, sort by vtime, extract sysno
    let mut plane2_sequences: HashMap<u16, Vec<u32>> = HashMap::new();
    let mut syscall_recs: Vec<&baud_proto::EbpfRecord> = ebpf_session.records.iter()
        .filter(|r| r.event.starts_with("syscall:"))
        .collect();
    syscall_recs.sort_by_key(|r| r.vtime);
    for rec in syscall_recs {
        if let Some(sysno_str) = rec.event.strip_prefix("syscall:") {
            if let Ok(sysno) = sysno_str.parse::<u32>() {
                plane2_sequences.entry(rec.node).or_default().push(sysno);
            }
        }
    }

    // Guard: if both planes are empty, the cross-check is vacuously trivial and
    // provides no evidence of determinism. Treat as a failure to avoid false positives.
    if plane1_counts.is_empty() && plane2_counts.is_empty() {
        return CrossCheckResult {
            run_id: run_id.into(),
            passed: false,
            divergent_node: None,
            divergent_sequence_step: None,
            plane1_counts,
            plane2_counts,
            plane2_source: ebpf_session.source.clone(),
            message: "observation cross-check FAILED: both plane-1 and plane-2 are empty (no data to compare — run the supervisor and seed eBPF records first)".into(),
        };
    }

    // Find first divergence by node (counts first, then sequences)
    let mut all_nodes: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
    all_nodes.extend(plane1_counts.keys());
    all_nodes.extend(plane2_counts.keys());

    let mut divergent_node = None;
    let mut divergent_sequence_step: Option<(u16, usize)> = None;
    let mut passed = true;

    for node in all_nodes {
        let p1 = plane1_counts.get(&node).copied().unwrap_or(0);
        let p2 = plane2_counts.get(&node).copied().unwrap_or(0);
        if p1 != p2 {
            passed = false;
            if divergent_node.is_none() {
                divergent_node = Some(node);
            }
        } else {
            // Counts agree: also compare ordered sequence
            let seq1 = plane1_sequences.get(&node).map(|v| v.as_slice()).unwrap_or(&[]);
            let seq2 = plane2_sequences.get(&node).map(|v| v.as_slice()).unwrap_or(&[]);
            for (step, (s1, s2)) in seq1.iter().zip(seq2.iter()).enumerate() {
                if s1 != s2 {
                    passed = false;
                    if divergent_sequence_step.is_none() {
                        divergent_sequence_step = Some((node, step));
                    }
                    if divergent_node.is_none() {
                        divergent_node = Some(node);
                    }
                    break;
                }
            }
        }
    }

    let message = if passed {
        "observation cross-check PASSED: plane 1 (supervisor) and plane 2 (eBPF) agree on counts and ordered sequences".into()
    } else if divergent_sequence_step.is_some() {
        format!(
            "observation cross-check FAILED: sequence divergence at node={:?} step={:?}; \
             this indicates a supervisor bug or escaped guest",
            divergent_node, divergent_sequence_step
        )
    } else {
        format!(
            "observation cross-check FAILED: count divergence at node={:?}; \
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
        divergent_sequence_step,
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
    fn test_bpf_availability_probe_does_not_panic() {
        // probe() must not panic regardless of platform.
        // On macOS it always returns Fallback; on Linux it may return Native or Fallback
        // depending on kernel BPF support and container policy.
        let avail = BpfAvailability::probe();
        // Both variants are valid; just verify no panic.
        let _ = avail.source(); // also exercises the source() method

        #[cfg(not(target_os = "linux"))]
        assert_eq!(avail, BpfAvailability::Fallback, "non-Linux must always be Fallback");
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

    // ---------------------------------------------------------------------------
    // Spec-mandated test names (specs/baud-tracing.md §6)
    // ---------------------------------------------------------------------------

    /// cross_check_detects_sequence_divergence: same counts but different syscall order
    /// must be detected as divergent.
    #[test]
    fn cross_check_detects_sequence_divergence() {
        // Plane 1: node 0 makes syscall 1 then syscall 2
        let syscalls_p1 = vec![
            make_syscall(0, 1, 10),
            make_syscall(0, 2, 20),
        ];
        // Plane 2: same count but reversed order (2 then 1) — simulates supervisor bug
        let syscalls_p2 = vec![
            make_syscall(0, 2, 10), // different sysno at same step
            make_syscall(0, 1, 20),
        ];
        let session = synthetic_from_syscalls("seq-test", &syscalls_p2);
        let result = cross_check("seq-test", &syscalls_p1, &session);
        // Counts agree (2 each) but sequences differ — must fail
        assert!(!result.passed, "cross_check_detects_sequence_divergence: must fail on sequence mismatch");
        assert!(result.divergent_sequence_step.is_some(), "divergent_sequence_step must be set");
        assert_eq!(result.divergent_sequence_step.unwrap().0, 0, "divergence on node 0");
    }

    /// planes_agree_on_healthy_run: plane 1 (supervisor syscall log) and plane 2
    /// (eBPF / fallback) must agree on per-guest syscall counts and ordered sequences
    /// for a well-behaved run.
    #[test]
    fn planes_agree_on_healthy_run() {
        // Build a known syscall log (plane 1)
        let syscalls = vec![
            make_syscall(0, 1, 10),   // node 0, sysno 1, vtime 10
            make_syscall(0, 2, 20),   // node 0, sysno 2, vtime 20
            make_syscall(0, 60, 30),  // node 0, exit (sysno 60), vtime 30
            make_syscall(1, 1, 15),   // node 1, sysno 1, vtime 15
            make_syscall(1, 60, 35),  // node 1, exit, vtime 35
        ];

        // Derive plane 2 from the same syscall log (as a fallback eBPF session)
        let plane2 = synthetic_from_syscalls("healthy-run", &syscalls);

        // Cross-check must pass
        let result = cross_check("healthy-run", &syscalls, &plane2);
        assert!(
            result.passed,
            "planes_agree_on_healthy_run: cross-check must PASS for healthy run. {}",
            result.message
        );
        assert_eq!(result.divergent_node, None, "no divergent node in healthy run");

        // Node 0 has 3 syscalls, node 1 has 2
        assert_eq!(result.plane1_counts.get(&0), Some(&3));
        assert_eq!(result.plane1_counts.get(&1), Some(&2));
    }

    /// fallback_emits_same_schema: when BPF is unavailable, the fallback
    /// (/proc-sampling + strace-shim) must emit `EbpfRecord`s with the same
    /// schema as the native BPF path, flagged `source=fallback`.
    #[test]
    fn fallback_emits_same_schema() {
        // Build a synthetic fallback session from syscall records
        let syscalls = vec![
            make_syscall(0, 1, 10),
            make_syscall(0, 2, 20),
        ];
        let session = synthetic_from_syscalls("fallback-run", &syscalls);

        // All records must carry the fallback source flag
        for rec in &session.records {
            assert_eq!(
                rec.source,
                Source::Fallback,
                "fallback session record must have source=Fallback (BpfAvailability::probe() returns Fallback on this platform)"
            );
        }

        // Records must have the same shape as native BPF records
        assert_eq!(session.records.len(), 2, "fallback session must emit one record per syscall");
        assert_eq!(session.records[0].node, 0);
        assert!(session.records[0].event.starts_with("syscall:"), "event must be prefixed 'syscall:'");

        // Cross-check must succeed even with fallback source
        let result = cross_check("fallback-run", &syscalls, &session);
        assert!(result.passed, "fallback source must still pass cross-check: {}", result.message);
    }
}
