// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Backend trait — the abstraction over Daytona and local backends.
// Nothing above this trait may import baud-tape or baud-tape-local directly.

use std::path::Path;
use anyhow::Result;
use async_trait::async_trait;
use crate::types::{ExecResult, SandboxSpec, SandboxStatus};

/// The Backend trait: create/destroy/exec/put/get/status/endpoint.
///
/// Implementations:
/// - `baud-tape::DaytonaBackend` — Daytona cloud API
/// - `baud-tape-local::LocalBackend` — local Linux subprocess / lima VM
///
/// One shared conformance test suite runs against both; a feature that works
/// on only one backend fails CI.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Create a new sandbox. Returns the sandbox ID.
    async fn create(&self, spec: &SandboxSpec) -> Result<String>;

    /// Start a stopped sandbox (revive after auto-stop).
    async fn start(&self, id: &str) -> Result<()>;

    /// Stop a running sandbox.
    async fn stop(&self, id: &str) -> Result<()>;

    /// Restore an archived sandbox (revive after auto-archive).
    async fn restore(&self, id: &str) -> Result<()>;

    /// Permanently delete a sandbox.
    async fn delete(&self, id: &str) -> Result<()>;

    /// Get current status of a sandbox.
    async fn status(&self, id: &str) -> Result<SandboxStatus>;

    /// Execute a command inside a sandbox. Returns stdout/stderr/exit code.
    async fn exec(&self, id: &str, cmd: &[&str]) -> Result<ExecResult>;

    /// Upload a file into the sandbox at the given remote path.
    async fn put(&self, id: &str, remote_path: &Path, data: &[u8]) -> Result<()>;

    /// Download a file from the sandbox at the given remote path.
    async fn get(&self, id: &str, remote_path: &Path) -> Result<Vec<u8>>;

    /// Get the preview/access URL for the sandbox, if available.
    async fn endpoint(&self, id: &str) -> Result<Option<String>>;

    /// Ensure a sandbox is running: start it if stopped, restore if archived.
    /// Returns the final status.
    async fn ensure(&self, id: &str) -> Result<SandboxStatus> {
        let s = self.status(id).await?;
        match s.state {
            crate::types::TapeState::Running => {}
            crate::types::TapeState::Stopped => {
                self.start(id).await?;
            }
            crate::types::TapeState::Archived => {
                self.restore(id).await?;
            }
            ref state => {
                anyhow::bail!("cannot ensure sandbox {id}: state is {state}");
            }
        }
        self.status(id).await
    }
}

// ---------------------------------------------------------------------------
// Conformance test suite (runs against both backends)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod conformance {
    use super::*;
    use crate::types::{SandboxSpec, TapeState};

    /// Run the conformance suite against a backend.
    /// Backends must have a 1-minute auto-stop timer so we can test that.
    /// WARNING: this creates real sandboxes and incurs cost on Daytona backend.
    pub async fn run_conformance(backend: &dyn Backend) -> Result<()> {
        let spec = SandboxSpec::default();

        // 1. Create
        let id = backend.create(&spec).await?;
        println!("conformance: created sandbox {id}");

        // 2. Status shows running with correct specs
        let status = backend.status(&id).await?;
        assert_eq!(status.vcpus, 1, "vcpus must be 1");
        assert!(status.memory_mib >= 1024, "memory must be >= 1 GiB");
        assert!(status.disk_mib >= 1024, "disk must be >= 1 GiB");
        assert_eq!(status.auto_stop_secs, 60, "auto-stop must be 1 minute");
        assert_eq!(status.auto_archive_secs, 300, "auto-archive must be 5 minutes");

        // 3. Exec echo
        let result = backend.exec(&id, &["echo", "hello"]).await?;
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello"), "stdout: {}", result.stdout);
        println!("conformance: exec echo ok");

        // 4. File put/get
        let content = b"baud conformance test file";
        let remote = std::path::Path::new("/tmp/baud-conformance.txt");
        backend.put(&id, remote, content).await?;
        let got = backend.get(&id, remote).await?;
        assert_eq!(&got, content, "file content mismatch");
        println!("conformance: put/get ok");

        // 5. Delete / kill
        backend.delete(&id).await?;
        println!("conformance: delete ok");

        println!("conformance: PASSED");
        Ok(())
    }
}
