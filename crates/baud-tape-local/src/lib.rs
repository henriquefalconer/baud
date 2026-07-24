// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-tape-local — the Backend trait as a local Linux subprocess.
//
// On macOS dev machines: uses a lima/colima VM (documented in `doctor`).
// On Linux: runs processes directly in temp directories.
//
// Exists so CI and integration tests run without cloud or cost.
// One shared conformance test suite runs against both backends;
// a feature that works on only one backend fails CI.
//
// The local backend models the same lifecycle as Daytona:
// - create: allocate a temp directory and record state
// - start/stop/restore: transition state
// - delete: remove the temp directory
// - exec: run a shell command in the temp directory
// - put/get: write/read files in the temp directory
// - auto-stop: after 60s idle, transition to Stopped (tracked via timer)
// - auto-archive: after 300s in Stopped, transition to Archived

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use baud_tape::types::{ExecResult, SandboxSpec, SandboxStatus, TapeState};
use baud_tape::Backend;
use tokio::sync::Mutex;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Internal state per sandbox
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SandboxEntry {
    id: String,
    spec: SandboxSpec,
    state: TapeState,
    root: PathBuf,
    created_at: u64,
    /// Last activity time (for auto-stop tracking)
    last_active: Instant,
    /// When the sandbox entered Stopped state (for auto-archive tracking)
    stopped_at: Option<Instant>,
}

impl SandboxEntry {
    fn status(&self) -> SandboxStatus {
        SandboxStatus {
            id: self.id.clone(),
            state: self.state.clone(),
            vcpus: self.spec.vcpus,
            memory_mib: self.spec.memory_mib,
            disk_mib: self.spec.disk_mib,
            auto_stop_secs: self.spec.auto_stop_secs,
            auto_archive_secs: self.spec.auto_archive_secs,
            preview_url: None,
            created_at: self.created_at,
        }
    }

    /// Advance timers: check auto-stop and auto-archive thresholds.
    fn tick(&mut self) {
        let now = Instant::now();
        match self.state {
            TapeState::Running => {
                let idle = now.duration_since(self.last_active);
                if idle > Duration::from_secs(self.spec.auto_stop_secs as u64) {
                    info!("local: sandbox {} auto-stopped after {}s idle", self.id, idle.as_secs());
                    self.state = TapeState::Stopped;
                    self.stopped_at = Some(now);
                }
            }
            TapeState::Stopped => {
                if let Some(stopped_at) = self.stopped_at {
                    let stopped_for = now.duration_since(stopped_at);
                    if stopped_for > Duration::from_secs(self.spec.auto_archive_secs as u64) {
                        info!("local: sandbox {} auto-archived after {}s stopped", self.id, stopped_for.as_secs());
                        self.state = TapeState::Archived;
                        self.stopped_at = None;
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// LocalBackend
// ---------------------------------------------------------------------------

/// A Backend implementation that runs sandboxes as local directories + processes.
///
/// On macOS, exec commands are forwarded into a lima VM if available;
/// otherwise they run on the host (useful for pure file-level operations and tests).
pub struct LocalBackend {
    sandboxes: Arc<Mutex<HashMap<String, SandboxEntry>>>,
    /// Base directory for sandbox roots
    base_dir: PathBuf,
    /// On macOS: use lima VM for exec
    use_lima: bool,
    /// Lima VM name
    lima_vm: String,
}

impl LocalBackend {
    /// Create a local backend using the system temp directory.
    pub fn new() -> Self {
        let base_dir = std::env::temp_dir().join("baud-local");
        LocalBackend {
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
            base_dir,
            use_lima: detect_lima(),
            lima_vm: "baud".to_owned(),
        }
    }

    /// Create a local backend with a specific base directory.
    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        LocalBackend {
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
            base_dir,
            use_lima: detect_lima(),
            lima_vm: "baud".to_owned(),
        }
    }

    /// Set the lima VM name (default: "baud").
    pub fn with_lima_vm(mut self, name: &str) -> Self {
        self.lima_vm = name.to_owned();
        self
    }

    async fn get_sandbox(&self, id: &str) -> Result<SandboxEntry> {
        let mut map = self.sandboxes.lock().await;
        let entry = map.get_mut(id)
            .with_context(|| format!("sandbox {id} not found"))?;
        entry.tick();
        Ok(entry.clone())
    }

    async fn mutate_sandbox<F>(&self, id: &str, f: F) -> Result<SandboxEntry>
    where
        F: FnOnce(&mut SandboxEntry) -> Result<()>,
    {
        let mut map = self.sandboxes.lock().await;
        let entry = map.get_mut(id)
            .with_context(|| format!("sandbox {id} not found"))?;
        entry.tick();
        f(entry)?;
        Ok(entry.clone())
    }

    fn sandbox_root(&self, id: &str) -> PathBuf {
        self.base_dir.join(id)
    }

    async fn run_cmd(&self, id: &str, cmd: &[&str]) -> Result<ExecResult> {
        // On Linux or when no lima: run directly via /bin/sh
        // On macOS with lima: forward via limactl shell
        if self.use_lima && cfg!(target_os = "macos") {
            let shell_cmd = cmd.join(" ");
            let cwd = self.sandbox_root(id);
            let out = tokio::process::Command::new("limactl")
                .args(["shell", &self.lima_vm, "sh", "-c", &shell_cmd])
                .current_dir(&cwd)
                .output()
                .await
                .context("limactl exec failed")?;
            Ok(ExecResult {
                exit_code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            })
        } else {
            let shell_cmd = cmd.join(" ");
            let cwd = self.sandbox_root(id);
            let out = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&shell_cmd)
                .current_dir(&cwd)
                .output()
                .await
                .context("sh exec failed")?;
            Ok(ExecResult {
                exit_code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            })
        }
    }
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn detect_lima() -> bool {
    // Check if limactl is available
    std::process::Command::new("limactl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[async_trait]
impl Backend for LocalBackend {
    async fn create(&self, spec: &SandboxSpec) -> Result<String> {
        let id = format!("local-{}", uuid::Uuid::new_v4().to_string().split('-').next().unwrap());
        let root = self.sandbox_root(&id);
        tokio::fs::create_dir_all(&root).await
            .with_context(|| format!("create sandbox dir {root:?}"))?;
        // Create standard directories
        tokio::fs::create_dir_all(root.join("tmp")).await?;
        tokio::fs::create_dir_all(root.join("run")).await?;

        let entry = SandboxEntry {
            id: id.clone(),
            spec: spec.clone(),
            state: TapeState::Running,
            root,
            created_at: unix_now(),
            last_active: Instant::now(),
            stopped_at: None,
        };

        let mut map = self.sandboxes.lock().await;
        map.insert(id.clone(), entry);
        info!("local: created sandbox {id}");
        Ok(id)
    }

    async fn start(&self, id: &str) -> Result<()> {
        self.mutate_sandbox(id, |e| {
            match e.state {
                TapeState::Stopped => {
                    e.state = TapeState::Running;
                    e.last_active = Instant::now();
                    e.stopped_at = None;
                    Ok(())
                }
                ref s => bail!("cannot start sandbox in state {s}"),
            }
        }).await?;
        info!("local: started sandbox {id}");
        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<()> {
        self.mutate_sandbox(id, |e| {
            match e.state {
                TapeState::Running => {
                    e.state = TapeState::Stopped;
                    e.stopped_at = Some(Instant::now());
                    Ok(())
                }
                ref s => bail!("cannot stop sandbox in state {s}"),
            }
        }).await?;
        info!("local: stopped sandbox {id}");
        Ok(())
    }

    async fn restore(&self, id: &str) -> Result<()> {
        self.mutate_sandbox(id, |e| {
            match e.state {
                TapeState::Archived | TapeState::Stopped => {
                    e.state = TapeState::Running;
                    e.last_active = Instant::now();
                    e.stopped_at = None;
                    Ok(())
                }
                ref s => bail!("cannot restore sandbox in state {s}"),
            }
        }).await?;
        info!("local: restored sandbox {id}");
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let root = {
            let mut map = self.sandboxes.lock().await;
            let entry = map.remove(id)
                .with_context(|| format!("sandbox {id} not found"))?;
            entry.root
        };
        if root.exists() {
            tokio::fs::remove_dir_all(&root).await
                .with_context(|| format!("remove sandbox dir {root:?}"))?;
        }
        info!("local: deleted sandbox {id}");
        Ok(())
    }

    async fn status(&self, id: &str) -> Result<SandboxStatus> {
        let entry = self.get_sandbox(id).await?;
        Ok(entry.status())
    }

    async fn exec(&self, id: &str, cmd: &[&str]) -> Result<ExecResult> {
        // Check sandbox is accessible
        {
            let entry = self.get_sandbox(id).await?;
            if entry.state == TapeState::Deleted {
                bail!("sandbox {id} is deleted");
            }
        }
        // Mark active
        self.mutate_sandbox(id, |e| {
            e.last_active = Instant::now();
            Ok(())
        }).await?;

        let result = self.run_cmd(id, cmd).await?;
        debug!("local exec [{id}] {:?} → exit {}", cmd, result.exit_code);
        Ok(result)
    }

    async fn put(&self, id: &str, remote_path: &Path, data: &[u8]) -> Result<()> {
        let entry = self.get_sandbox(id).await?;
        // Map /tmp/... to sandbox_root/tmp/...
        let local_path = map_path(&entry.root, remote_path);
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&local_path, data).await
            .with_context(|| format!("write {local_path:?}"))?;
        debug!("local put [{id}] {remote_path:?} ({} bytes)", data.len());
        Ok(())
    }

    async fn get(&self, id: &str, remote_path: &Path) -> Result<Vec<u8>> {
        let entry = self.get_sandbox(id).await?;
        let local_path = map_path(&entry.root, remote_path);
        let data = tokio::fs::read(&local_path).await
            .with_context(|| format!("read {local_path:?}"))?;
        debug!("local get [{id}] {remote_path:?} ({} bytes)", data.len());
        Ok(data)
    }

    async fn endpoint(&self, _id: &str) -> Result<Option<String>> {
        // Local backend has no network endpoint
        Ok(None)
    }
}

/// Map an absolute-looking path into the sandbox root.
/// /tmp/foo → {sandbox_root}/tmp/foo
/// /run/foo → {sandbox_root}/run/foo
/// /foo/bar → {sandbox_root}/foo/bar
fn map_path(root: &Path, remote: &Path) -> PathBuf {
    // Strip leading '/' from the remote path and join under root
    let relative = remote.strip_prefix("/").unwrap_or(remote);
    root.join(relative)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use baud_tape::types::SandboxSpec;
    use baud_tape::Backend;
    use std::path::Path;

    async fn make_backend() -> LocalBackend {
        let tmp = std::env::temp_dir().join(format!("baud-test-{}", uuid::Uuid::new_v4()));
        LocalBackend::with_base_dir(tmp)
    }

    #[tokio::test]
    async fn create_and_status() {
        let b = make_backend().await;
        let spec = SandboxSpec::default();
        let id = b.create(&spec).await.expect("create");
        let status = b.status(&id).await.expect("status");
        assert_eq!(status.id, id);
        assert_eq!(status.state, TapeState::Running);
        assert_eq!(status.vcpus, 1);
        assert_eq!(status.memory_mib, 1024);
    }

    #[tokio::test]
    async fn exec_echo() {
        let b = make_backend().await;
        let spec = SandboxSpec::default();
        let id = b.create(&spec).await.expect("create");
        let r = b.exec(&id, &["echo", "hello baud"]).await.expect("exec");
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("hello baud"), "stdout: {}", r.stdout);
    }

    #[tokio::test]
    async fn put_and_get_file() {
        let b = make_backend().await;
        let spec = SandboxSpec::default();
        let id = b.create(&spec).await.expect("create");
        let content = b"baud test content";
        b.put(&id, Path::new("/tmp/test.txt"), content).await.expect("put");
        let got = b.get(&id, Path::new("/tmp/test.txt")).await.expect("get");
        assert_eq!(&got, content);
    }

    #[tokio::test]
    async fn stop_and_start() {
        let b = make_backend().await;
        let spec = SandboxSpec::default();
        let id = b.create(&spec).await.expect("create");

        b.stop(&id).await.expect("stop");
        let s = b.status(&id).await.expect("status after stop");
        assert_eq!(s.state, TapeState::Stopped);

        b.start(&id).await.expect("start");
        let s = b.status(&id).await.expect("status after start");
        assert_eq!(s.state, TapeState::Running);
    }

    #[tokio::test]
    async fn ensure_from_stopped() {
        let b = make_backend().await;
        let spec = SandboxSpec::default();
        let id = b.create(&spec).await.expect("create");
        b.stop(&id).await.expect("stop");
        let s = b.ensure(&id).await.expect("ensure");
        assert_eq!(s.state, TapeState::Running);
    }

    #[tokio::test]
    async fn ensure_from_archived() {
        let b = make_backend().await;
        let spec = SandboxSpec::default();
        let id = b.create(&spec).await.expect("create");
        // Manually set to archived
        {
            let mut map = b.sandboxes.lock().await;
            map.get_mut(&id).unwrap().state = TapeState::Archived;
        }
        let s = b.ensure(&id).await.expect("ensure from archived");
        assert_eq!(s.state, TapeState::Running);
    }

    #[tokio::test]
    async fn delete_removes_sandbox() {
        let b = make_backend().await;
        let spec = SandboxSpec::default();
        let id = b.create(&spec).await.expect("create");
        b.delete(&id).await.expect("delete");
        let r = b.status(&id).await;
        assert!(r.is_err(), "status after delete should fail");
    }

    #[tokio::test]
    async fn auto_stop_timer() {
        let b = make_backend().await;
        // Use a very short auto-stop
        let spec = SandboxSpec {
            auto_stop_secs: 0, // instant
            ..Default::default()
        };
        let id = b.create(&spec).await.expect("create");
        // Sleep briefly so the instant timer fires
        tokio::time::sleep(Duration::from_millis(10)).await;
        let s = b.status(&id).await.expect("status");
        assert_eq!(s.state, TapeState::Stopped, "should be auto-stopped");
    }
}
