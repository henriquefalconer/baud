// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use serde::{Deserialize, Serialize};

/// State of a tape (sandbox instance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeState {
    /// Sandbox is being created or provisioned
    Creating,
    /// Sandbox is running and accepting commands
    Running,
    /// Sandbox has auto-stopped after the 1-minute idle timer
    Stopped,
    /// Sandbox has been archived after the 5-minute timer
    Archived,
    /// Sandbox has been permanently deleted
    Deleted,
    /// Unknown state (API returned something unexpected)
    Unknown(String),
}

impl std::fmt::Display for TapeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TapeState::Creating => write!(f, "creating"),
            TapeState::Running => write!(f, "running"),
            TapeState::Stopped => write!(f, "stopped"),
            TapeState::Archived => write!(f, "archived"),
            TapeState::Deleted => write!(f, "deleted"),
            TapeState::Unknown(s) => write!(f, "unknown({s})"),
        }
    }
}

/// Specification for creating a new sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Number of vCPUs (must be 1 per hard constraints)
    pub vcpus: u32,
    /// RAM in MiB (must be 1024 per hard constraints)
    pub memory_mib: u32,
    /// Disk in MiB (must be 1024 per hard constraints; fall back to platform minimum)
    pub disk_mib: u32,
    /// Auto-stop after idle (seconds; must be 60 per hard constraints)
    pub auto_stop_secs: u32,
    /// Auto-archive after stop (seconds; must be 300 per hard constraints)
    pub auto_archive_secs: u32,
    /// Optional image/snapshot ID to boot from
    pub image: Option<String>,
    /// Labels for identification
    pub labels: std::collections::HashMap<String, String>,
}

impl Default for SandboxSpec {
    fn default() -> Self {
        SandboxSpec {
            vcpus: 1,
            memory_mib: 1024,
            disk_mib: 1024,
            auto_stop_secs: 60,
            auto_archive_secs: 300,
            image: None,
            labels: Default::default(),
        }
    }
}

/// Status information about a running sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxStatus {
    /// Unique sandbox ID assigned by the backend
    pub id: String,
    /// Current lifecycle state
    pub state: TapeState,
    /// Number of vCPUs (as reported by backend; should be 1)
    pub vcpus: u32,
    /// RAM in MiB
    pub memory_mib: u32,
    /// Disk in MiB (may differ from requested if platform minimum is higher)
    pub disk_mib: u32,
    /// Auto-stop timeout in seconds
    pub auto_stop_secs: u32,
    /// Auto-archive timeout in seconds
    pub auto_archive_secs: u32,
    /// Preview / access URL (if available)
    pub preview_url: Option<String>,
    /// Creation timestamp (unix seconds)
    pub created_at: u64,
}

/// Result of executing a command inside a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    /// Exit code (0 = success)
    pub exit_code: i32,
    /// Standard output
    pub stdout: String,
    /// Standard error
    pub stderr: String,
}
