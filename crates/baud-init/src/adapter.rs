// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Closed adapter set for baud-init.

use serde::{Deserialize, Serialize};
use anyhow::{bail, Result};

// ---------------------------------------------------------------------------
// Input adapters
// ---------------------------------------------------------------------------

/// Input adapter — how tape-derived bytes reach a guest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputAdapter {
    /// Tape-derived bytes to the guest's stdin
    Stdin,
    /// Tape-derived bytes to a named pipe the guest reads
    Fifo { path: String },
    /// Messages via the supervisor's virtual net device
    Net,
}

// ---------------------------------------------------------------------------
// Probe adapters
// ---------------------------------------------------------------------------

/// How to read virtual-fs file content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VfsMode {
    Hash,
    U64,
    Utf8,
}

/// Probe adapter — how observations are extracted from a guest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeAdapter {
    /// Parse `key=value` lines from guest stdout (optional prefix filter)
    StdoutKv { prefix: Option<String> },
    /// Read a virtual-fs file
    VfsFile { path: String, mode: VfsMode },
    /// Count matching syscalls from plane 1
    SyscallCounter { pattern: String },
    /// Count kernel events from plane 2 (eBPF)
    EbpfCounter { event: String },
    /// Final-state hash from the exit device
    ExitHash,
}

// ---------------------------------------------------------------------------
// Display adapters
// ---------------------------------------------------------------------------

/// Raw frame format for graphical surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameFormat {
    Rgba8888,
    Rgb565,
    Indexed8,
}

/// Transport for frame buffers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameTransport {
    Fifo,
    Vfs,
}

/// Display adapter — how a guest exposes a graphical surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameAdapter {
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    pub transport: FrameTransport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayAdapter {
    Frame(FrameAdapter),
}

// ---------------------------------------------------------------------------
// Combined adapter collection for a node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Adapter {
    pub input: Option<InputAdapter>,
    pub probes: Vec<ProbeAdapter>,
    pub display: Option<DisplayAdapter>,
}

// ---------------------------------------------------------------------------
// Parsing from serde_yaml::Value
// ---------------------------------------------------------------------------

pub fn parse_input_adapter(v: &serde_yaml::Value) -> Result<InputAdapter> {
    match v {
        serde_yaml::Value::String(s) => match s.as_str() {
            "stdin" => Ok(InputAdapter::Stdin),
            "net" => Ok(InputAdapter::Net),
            other => bail!("unknown input adapter: {other}; valid: stdin, net, fifo{{path}}"),
        },
        serde_yaml::Value::Mapping(m) => {
            if let Some(fifo) = m.get("fifo") {
                let path = fifo
                    .get("path")
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| anyhow::anyhow!("fifo adapter requires 'path'"))?
                    .to_string();
                Ok(InputAdapter::Fifo { path })
            } else {
                bail!("unknown input adapter mapping; valid keys: fifo")
            }
        }
        _ => bail!("input adapter must be a string or mapping"),
    }
}

pub fn parse_probe_adapter(v: &serde_yaml::Value) -> Result<ProbeAdapter> {
    match v {
        serde_yaml::Value::String(s) => match s.as_str() {
            "stdout-kv" => Ok(ProbeAdapter::StdoutKv { prefix: None }),
            "exit-hash" => Ok(ProbeAdapter::ExitHash),
            other => bail!(
                "unknown probe adapter '{other}'; valid: stdout-kv, vfs-file, \
                 syscall-counter, ebpf-counter, exit-hash"
            ),
        },
        serde_yaml::Value::Mapping(m) => {
            if let Some(kv) = m.get("stdout-kv") {
                let prefix = kv
                    .get("prefix")
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string());
                Ok(ProbeAdapter::StdoutKv { prefix })
            } else if let Some(vfs) = m.get("vfs-file") {
                let path = vfs
                    .get("path")
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| anyhow::anyhow!("vfs-file requires 'path'"))?
                    .to_string();
                let mode_str = vfs
                    .get("mode")
                    .and_then(|m| m.as_str())
                    .unwrap_or("utf8");
                let mode = match mode_str {
                    "hash" => VfsMode::Hash,
                    "u64" => VfsMode::U64,
                    "utf8" => VfsMode::Utf8,
                    other => bail!("unknown vfs-file mode '{other}'; valid: hash, u64, utf8"),
                };
                Ok(ProbeAdapter::VfsFile { path, mode })
            } else if let Some(sc) = m.get("syscall-counter") {
                let pattern = sc
                    .get("pattern")
                    .or_else(|| sc.get("sysno"))
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| anyhow::anyhow!("syscall-counter requires 'pattern' or 'sysno'"))?
                    .to_string();
                Ok(ProbeAdapter::SyscallCounter { pattern })
            } else if let Some(ebpf) = m.get("ebpf-counter") {
                let event = ebpf
                    .get("event")
                    .and_then(|e| e.as_str())
                    .ok_or_else(|| anyhow::anyhow!("ebpf-counter requires 'event'"))?
                    .to_string();
                Ok(ProbeAdapter::EbpfCounter { event })
            } else {
                bail!(
                    "unknown probe adapter mapping; valid keys: stdout-kv, vfs-file, \
                     syscall-counter, ebpf-counter"
                )
            }
        }
        _ => bail!("probe adapter must be a string or mapping"),
    }
}

pub fn parse_display_adapter(v: &serde_yaml::Value) -> Result<DisplayAdapter> {
    match v {
        serde_yaml::Value::Mapping(m) => {
            if let Some(frame) = m.get("frame") {
                let width = frame
                    .get("width")
                    .and_then(|w| w.as_u64())
                    .ok_or_else(|| anyhow::anyhow!("frame requires 'width'"))?
                    as u32;
                let height = frame
                    .get("height")
                    .and_then(|h| h.as_u64())
                    .ok_or_else(|| anyhow::anyhow!("frame requires 'height'"))?
                    as u32;
                let format_str = frame
                    .get("format")
                    .and_then(|f| f.as_str())
                    .ok_or_else(|| anyhow::anyhow!("frame requires 'format'"))?;
                let format = match format_str {
                    "rgba8888" => FrameFormat::Rgba8888,
                    "rgb565" => FrameFormat::Rgb565,
                    "indexed8" => FrameFormat::Indexed8,
                    other => bail!(
                        "unknown frame format '{other}'; valid: rgba8888, rgb565, indexed8"
                    ),
                };
                let transport_str = frame
                    .get("transport")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| anyhow::anyhow!("frame requires 'transport'"))?;
                let transport = match transport_str {
                    "fifo" => FrameTransport::Fifo,
                    "vfs" => FrameTransport::Vfs,
                    other => bail!(
                        "unknown frame transport '{other}'; valid: fifo, vfs"
                    ),
                };
                Ok(DisplayAdapter::Frame(FrameAdapter {
                    width,
                    height,
                    format,
                    transport,
                }))
            } else {
                bail!("unknown display adapter; valid keys: frame")
            }
        }
        _ => bail!("display adapter must be a mapping"),
    }
}

pub fn parse_adapters(v: &serde_yaml::Value) -> Result<Adapter> {
    let m = match v {
        serde_yaml::Value::Mapping(m) => m,
        _ => bail!("adapters must be a mapping"),
    };

    let mut adapter = Adapter::default();

    for (k, val) in m {
        let key = k.as_str().ok_or_else(|| anyhow::anyhow!("adapter key must be a string"))?;
        match key {
            "input" => {
                adapter.input = Some(parse_input_adapter(val)?);
            }
            "probes" => {
                let probes = val
                    .as_sequence()
                    .ok_or_else(|| anyhow::anyhow!("adapters.probes must be a list"))?;
                for p in probes {
                    adapter.probes.push(parse_probe_adapter(p)?);
                }
            }
            "display" => {
                adapter.display = Some(parse_display_adapter(val)?);
            }
            other => bail!(
                "unknown adapter key '{other}'; valid: input, probes, display"
            ),
        }
    }

    Ok(adapter)
}
