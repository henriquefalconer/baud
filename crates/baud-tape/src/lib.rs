// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-tape — typed REST client for the Daytona API
//
// Wraps only the endpoints baud calls:
//   create/start/stop/archive/delete sandbox, exec, file upload/download,
//   preview URL
//
// Hidden behind the Backend trait shared with baud-tape-local.
// Nothing above the trait may import this crate.
//
// Retries with backoff; recorded-fixture contract tests.

pub mod backend;
pub mod daytona;
pub mod types;

pub use backend::Backend;
pub use types::{SandboxStatus, SandboxSpec, ExecResult, TapeState};

// ---------------------------------------------------------------------------
// Tests — conformance suite run against a stub backend (no cloud required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::backend::conformance;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use anyhow::{bail, Context, Result};
    use async_trait::async_trait;
    use tokio::sync::Mutex;
    use types::{SandboxSpec, SandboxStatus, TapeState, ExecResult};

    /// A minimal in-process stub backend for conformance testing.
    /// Models the same lifecycle as LocalBackend without I/O.
    struct StubBackend {
        sandboxes: Arc<Mutex<HashMap<String, (TapeState, HashMap<String, Vec<u8>>)>>>,
        counter: Arc<Mutex<u64>>,
    }

    impl StubBackend {
        fn new() -> Self {
            StubBackend {
                sandboxes: Arc::new(Mutex::new(HashMap::new())),
                counter: Arc::new(Mutex::new(0)),
            }
        }
    }

    #[async_trait]
    impl Backend for StubBackend {
        async fn create(&self, _spec: &SandboxSpec) -> Result<String> {
            let mut c = self.counter.lock().await;
            *c += 1;
            let id = format!("stub-{c}");
            let mut map = self.sandboxes.lock().await;
            map.insert(id.clone(), (TapeState::Running, HashMap::new()));
            Ok(id)
        }

        async fn start(&self, id: &str) -> Result<()> {
            let mut map = self.sandboxes.lock().await;
            let e = map.get_mut(id).context("not found")?;
            e.0 = TapeState::Running;
            Ok(())
        }

        async fn stop(&self, id: &str) -> Result<()> {
            let mut map = self.sandboxes.lock().await;
            let e = map.get_mut(id).context("not found")?;
            e.0 = TapeState::Stopped;
            Ok(())
        }

        async fn restore(&self, id: &str) -> Result<()> {
            let mut map = self.sandboxes.lock().await;
            let e = map.get_mut(id).context("not found")?;
            e.0 = TapeState::Running;
            Ok(())
        }

        async fn delete(&self, id: &str) -> Result<()> {
            let mut map = self.sandboxes.lock().await;
            if map.remove(id).is_none() {
                bail!("not found: {id}");
            }
            Ok(())
        }

        async fn status(&self, id: &str) -> Result<SandboxStatus> {
            let map = self.sandboxes.lock().await;
            let e = map.get(id).context("not found")?;
            Ok(SandboxStatus {
                id: id.into(),
                state: e.0.clone(),
                vcpus: 1,
                memory_mib: 1024,
                disk_mib: 1024,
                auto_stop_secs: 60,
                auto_archive_secs: 300,
                preview_url: None,
                created_at: 0,
            })
        }

        async fn exec(&self, id: &str, cmd: &[&str]) -> Result<ExecResult> {
            let map = self.sandboxes.lock().await;
            if !map.contains_key(id) {
                bail!("not found: {id}");
            }
            // Stub exec: handle "echo <args>" by returning joined args as stdout
            let stdout = if cmd.first() == Some(&"echo") {
                cmd[1..].join(" ") + "\n"
            } else {
                String::new()
            };
            Ok(ExecResult { exit_code: 0, stdout, stderr: String::new() })
        }

        async fn put(&self, id: &str, remote_path: &Path, data: &[u8]) -> Result<()> {
            let mut map = self.sandboxes.lock().await;
            let e = map.get_mut(id).context("not found")?;
            e.1.insert(remote_path.to_string_lossy().to_string(), data.to_vec());
            Ok(())
        }

        async fn get(&self, id: &str, remote_path: &Path) -> Result<Vec<u8>> {
            let map = self.sandboxes.lock().await;
            let e = map.get(id).context("not found")?;
            e.1.get(&remote_path.to_string_lossy().to_string())
                .cloned()
                .context("file not found")
        }

        async fn endpoint(&self, _id: &str) -> Result<Option<String>> {
            Ok(None)
        }
    }

    /// backend_conformance_parity (baud-tape): run the shared conformance suite
    /// against the StubBackend to verify the suite itself is callable from baud-tape.
    #[tokio::test]
    async fn backend_conformance_parity() {
        let b = StubBackend::new();
        conformance::run_conformance(&b)
            .await
            .expect("conformance suite must pass on StubBackend");
    }

    /// backend_lifecycle_conformance (baud-tape): run the extended lifecycle suite.
    #[tokio::test]
    async fn backend_lifecycle_conformance() {
        let b = StubBackend::new();
        conformance::run_lifecycle_conformance(&b)
            .await
            .expect("lifecycle conformance must pass on StubBackend");
    }
}
