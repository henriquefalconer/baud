// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The generic-core guardrail (todo.md §14 next-actions item 4, §11.0, §12 problem 29,
// specs/baud-guest-harness.md §8 `no_workload_specifics_in_core`): every crate under `crates/`
// stays generic; all workload knowledge (game/emulator symbols, RAM probe addresses) lives only
// under `examples/`. This scans `crates/` for terms that only make sense inside a specific
// example and fails loud if any leaked in.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Terms unique to the Mario/NES example (specs/baud-mario.md §4) that must never appear in
/// generic baud infrastructure. Bare "nes"/"smb" are deliberately excluded: as short substrings
/// they false-positive on ordinary identifiers (e.g. "guest_kernel", "openness"); every term
/// here is a compound identifier or a literal hex probe address specific enough that a real
/// match means real leakage, not noise.
pub const FORBIDDEN_WORKLOAD_TERMS: &[&str] = &[
    "fceux",
    "joypad",
    "harness.lua",
    "game.nes",
    "oper_mode",
    "super_mario",
    "mario_bros",
    "0x006d",
    "0x0086",
    "0x00ce",
    "0x075f",
    "0x0760",
    "0x075a",
    "0x0770",
];

/// A single occurrence of a forbidden workload-specific term inside a `crates/` source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadLeak {
    pub file: PathBuf,
    pub line: usize,
    pub term: String,
}

/// This lint's own defining file necessarily quotes every forbidden term as a string literal
/// (the list above), so it is excluded from its own scan rather than term-by-term special-cased.
const SELF_FILE_NAME: &str = "workload_lint.rs";

/// Scan every `.rs` file under `crates_dir` (recursing, skipping `target/`) for any
/// `FORBIDDEN_WORKLOAD_TERMS` occurrence, case-insensitively. An empty result means the generic
/// core stayed generic.
pub fn scan_crates_for_workload_leaks(crates_dir: &Path) -> Result<Vec<WorkloadLeak>> {
    let mut leaks = Vec::new();
    let mut stack = vec![crates_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .with_context(|| format!("reading directory {}", dir.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some(SELF_FILE_NAME) {
                continue;
            }
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            for (idx, line) in contents.lines().enumerate() {
                let lower = line.to_lowercase();
                for term in FORBIDDEN_WORKLOAD_TERMS {
                    if lower.contains(term) {
                        leaks.push(WorkloadLeak {
                            file: path.clone(),
                            line: idx + 1,
                            term: term.to_string(),
                        });
                    }
                }
            }
        }
    }
    leaks.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    Ok(leaks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_workload_specifics_in_core() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")); // crates/baud-packages
        let workspace_root = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("crates/baud-packages sits two levels under the workspace root");
        let crates_dir = workspace_root.join("crates");
        let leaks =
            scan_crates_for_workload_leaks(&crates_dir).expect("scanning crates/ for leaks");
        assert!(
            leaks.is_empty(),
            "workload-specific knowledge leaked into crates/ (belongs under examples/ instead): {:?}",
            leaks
        );
    }

    #[test]
    fn scan_detects_a_planted_leak() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_src = tmp.path().join("some-crate").join("src");
        fs::create_dir_all(&crate_src).unwrap();
        fs::write(
            crate_src.join("lib.rs"),
            "let x = mem[0x006D]; // fceux joypad read\n",
        )
        .unwrap();

        let leaks = scan_crates_for_workload_leaks(tmp.path()).unwrap();

        let terms: Vec<&str> = leaks.iter().map(|l| l.term.as_str()).collect();
        assert!(terms.contains(&"0x006d"));
        assert!(terms.contains(&"fceux"));
        assert!(terms.contains(&"joypad"));
    }

    #[test]
    fn scan_ignores_target_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let target_src = tmp.path().join("some-crate").join("target").join("debug");
        fs::create_dir_all(&target_src).unwrap();
        fs::write(target_src.join("build.rs"), "// fceux\n").unwrap();

        let leaks = scan_crates_for_workload_leaks(tmp.path()).unwrap();
        assert!(leaks.is_empty());
    }

    #[test]
    fn scan_ignores_its_own_defining_file() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_src = tmp.path().join("baud-packages").join("src");
        fs::create_dir_all(&crate_src).unwrap();
        fs::write(
            crate_src.join(SELF_FILE_NAME),
            "// this file quotes fceux/joypad in its own term list, on purpose\n",
        )
        .unwrap();

        let leaks = scan_crates_for_workload_leaks(tmp.path()).unwrap();
        assert!(leaks.is_empty());
    }
}
