// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// A reproducible newc-format initramfs builder (todo.md §4.3 / §14 next-actions item 1's "a
// reproducible initramfs builder ... driven by that pipeline rather than hand-built per a
// fixture's BUILD.md"). Every real guest fixture in this repo so far builds its initramfs by
// shelling out to the host's `cpio`/`gzip` by hand (see
// `crates/baud-multiverse/tests/fixtures/linux-guest/BUILD.md`'s "Regenerating the initramfs"
// recipe: `touch -h -d '@1'; find . -print0 | sort -z | cpio -o -H newc -R +0:+0 --reproducible
// --null | gzip -9n`). This module is that same recipe as real, tested Rust code with no
// dependency on the host having `cpio`/`gzip` installed at all -- the newc cpio format is written
// directly, and compression goes through this crate's own pinned `flate2`, so the output bytes are
// a pure function of the input entries plus the locked `flate2` version, not of whatever `gzip`
// happens to be on `$PATH`.

use std::collections::BTreeSet;
use std::io::Write;

use anyhow::{bail, Context, Result};
use flate2::{write::GzEncoder, Compression, GzBuilder};

/// One node to place in the initramfs, relative to the rootfs root -- e.g. `"init"`,
/// `"bin/harness"`, `"lib64/ld-linux-x86-64.so.2"`. No leading `/` or `./`, and no `..` component
/// (this builder only ever adds nodes under the root it is given, mirroring `find .` over a real
/// rootfs directory).
#[derive(Debug, Clone)]
pub struct InitramfsEntry {
    pub path: String,
    pub node: InitramfsNode,
}

/// What kind of filesystem node an [`InitramfsEntry`] is. Directory records are never listed here
/// -- they are synthesized automatically by [`build_reproducible_initramfs`] from every entry's
/// path prefix, exactly as a real `find .` walk would also produce them.
#[derive(Debug, Clone)]
pub enum InitramfsNode {
    /// A regular file. `mode` is permission bits only (e.g. `0o755`) -- the newc `S_IFREG`
    /// file-type bits are added by the builder, matching `find`'s own separation of "what kind of
    /// node" from "what mode bits".
    Regular { mode: u32, contents: Vec<u8> },
    /// A symbolic link. `target` is the raw link target string, stored verbatim as the newc
    /// record's data (no NUL terminator, matching real `cpio`'s own symlink encoding) -- needed
    /// for any real glibc/distro rootfs, whose dynamic linker and versioned shared objects are
    /// wired together almost entirely through symlinks (e.g. `/lib64/ld-linux-x86-64.so.2` is
    /// itself commonly a symlink to a path under `/lib/x86_64-linux-gnu/`).
    Symlink { target: String },
}

impl InitramfsEntry {
    pub fn regular(path: impl Into<String>, mode: u32, contents: Vec<u8>) -> Self {
        InitramfsEntry { path: path.into(), node: InitramfsNode::Regular { mode, contents } }
    }

    pub fn symlink(path: impl Into<String>, target: impl Into<String>) -> Self {
        InitramfsEntry { path: path.into(), node: InitramfsNode::Symlink { target: target.into() } }
    }
}

const NEWC_MAGIC: &[u8; 6] = b"070701";
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
/// Symlink permission bits `cpio`/`find` conventionally report (the kernel never checks a
/// symlink's own permission bits -- only the target's, resolved separately).
const SYMLINK_MODE: u32 = 0o777;
/// §4.3's fixed mtime (`touch -h -d '@1'`): every record gets the same, reproducible timestamp
/// rather than the real build wall-clock time.
const FIXED_MTIME: u32 = 1;

fn pad4(buf: &mut Vec<u8>) {
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

/// Write one newc cpio header + name + data record (data padded to a 4-byte boundary, per the
/// newc format spec) into `buf`.
fn write_record(buf: &mut Vec<u8>, ino: u32, mode: u32, nlink: u32, filesize: u32, name: &str, data: &[u8]) {
    let name_z = format!("{name}\0");
    buf.extend_from_slice(NEWC_MAGIC);
    let fields: [u32; 13] = [
        ino,
        mode,
        0, // uid -- always 0 (§4.3's `-R +0:+0`)
        0, // gid -- always 0
        nlink,
        FIXED_MTIME,
        filesize,
        0, // devmajor
        0, // devminor
        0, // rdevmajor
        0, // rdevminor
        name_z.len() as u32,
        0, // check -- unused by newc, always 0
    ];
    for field in fields {
        buf.extend_from_slice(format!("{field:08x}").as_bytes());
    }
    buf.extend_from_slice(name_z.as_bytes());
    pad4(buf);
    buf.extend_from_slice(data);
    pad4(buf);
}

/// Every directory implied by `path`'s components, excluding `path` itself -- mirrors what a real
/// `find .` walk over a rootfs directory would also emit as separate directory entries.
fn implied_dirs(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut out = Vec::new();
    let mut acc = String::new();
    for part in &parts[..parts.len().saturating_sub(1)] {
        acc = if acc.is_empty() { (*part).to_string() } else { format!("{acc}/{part}") };
        out.push(acc.clone());
    }
    out
}

/// Build a reproducible gzip-compressed newc cpio initramfs from `entries` -- §4.3's exact recipe
/// (fixed mtimes, uid/gid 0, sorted entries, `gzip -9n`), with no host `cpio`/`gzip` dependency.
///
/// Directory records are synthesized automatically for every path prefix (the root `.` plus any
/// intermediate directories `entries` implies), exactly as a real `find . | sort` walk over an
/// assembled rootfs directory would also produce them.
pub fn build_reproducible_initramfs(entries: &[InitramfsEntry]) -> Result<Vec<u8>> {
    let mut seen_paths = BTreeSet::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    dirs.insert(".".to_string());

    for entry in entries {
        if entry.path.is_empty() || entry.path.starts_with('/') || entry.path == "." {
            bail!(
                "initramfs entry path must be a non-empty relative path, not '/'-rooted or '.': {}",
                entry.path
            );
        }
        if entry.path.split('/').any(|component| component == "..") {
            bail!("initramfs entry path must not contain '..': {}", entry.path);
        }
        if !seen_paths.insert(entry.path.clone()) {
            bail!("duplicate initramfs entry path: {}", entry.path);
        }
        if let InitramfsNode::Symlink { target } = &entry.node {
            if target.is_empty() {
                bail!("symlink initramfs entry has an empty target: {}", entry.path);
            }
        }
        dirs.extend(implied_dirs(&entry.path));
    }

    // One flat, sorted list of (path, is_dir, mode, nlink, data) -- `find . | sort` order: "." is
    // the lowest byte value ('.' == 0x2e) among any real path, so the root directory always leads.
    let mut records: Vec<(&str, bool, u32, u32, &[u8])> = Vec::new();
    for dir in &dirs {
        records.push((dir.as_str(), true, S_IFDIR | 0o755, 2, &[]));
    }
    for entry in entries {
        let (mode, nlink, data): (u32, u32, &[u8]) = match &entry.node {
            InitramfsNode::Regular { mode, contents } => (S_IFREG | (mode & 0o7777), 1, contents.as_slice()),
            InitramfsNode::Symlink { target } => (S_IFLNK | SYMLINK_MODE, 1, target.as_bytes()),
        };
        records.push((entry.path.as_str(), false, mode, nlink, data));
    }
    records.sort_by(|a, b| a.0.cmp(b.0));

    let mut cpio = Vec::new();
    for (i, (path, _is_dir, mode, nlink, data)) in records.iter().enumerate() {
        let ino = (i + 1) as u32;
        write_record(&mut cpio, ino, *mode, *nlink, data.len() as u32, path, data);
    }
    write_record(&mut cpio, 0, 0, 1, 0, "TRAILER!!!", &[]);

    // `gzip -9n`: best compression, fixed mtime=0 (no filename/comment set either), so the header
    // carries no host- or wall-clock-derived bytes.
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    let gz_buf = write_all_and_finish(encoder, &cpio)
        .context("failed to gzip-compress the reproducible initramfs")?;
    Ok(gz_buf)
}

fn write_all_and_finish(mut encoder: GzEncoder<Vec<u8>>, data: &[u8]) -> std::io::Result<Vec<u8>> {
    encoder.write_all(data)?;
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Minimal newc-cpio reader, test-only: decodes exactly what `build_reproducible_initramfs`
    /// writes, so tests can assert on real structure rather than just "the bytes look the same".
    fn parse_newc_cpio(data: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        loop {
            assert_eq!(&data[pos..pos + 6], NEWC_MAGIC, "bad newc magic at offset {pos}");
            let field = |index: usize| -> u32 {
                let start = pos + 6 + index * 8;
                let text = std::str::from_utf8(&data[start..start + 8]).unwrap();
                u32::from_str_radix(text, 16).unwrap()
            };
            let mode = field(1);
            let filesize = field(6);
            let namesize = field(11);

            let name_start = pos + 110;
            let name_end = name_start + namesize as usize - 1; // exclude the NUL terminator
            let name = String::from_utf8(data[name_start..name_end].to_vec()).unwrap();

            let mut data_start = name_start + namesize as usize;
            while !data_start.is_multiple_of(4) {
                data_start += 1;
            }
            let data_end = data_start + filesize as usize;
            let contents = data[data_start..data_end].to_vec();

            pos = data_end;
            while !pos.is_multiple_of(4) {
                pos += 1;
            }

            if name == "TRAILER!!!" {
                break;
            }
            out.push((name, mode, contents));
        }
        out
    }

    fn gunzip(bytes: &[u8]) -> Vec<u8> {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).unwrap();
        out
    }

    fn init_entry(contents: &[u8]) -> InitramfsEntry {
        InitramfsEntry::regular("init", 0o755, contents.to_vec())
    }

    #[test]
    fn round_trip_preserves_a_single_file() {
        let gz = build_reproducible_initramfs(&[init_entry(b"#!static-init-binary")]).unwrap();
        let cpio = gunzip(&gz);
        let records = parse_newc_cpio(&cpio);

        let (name, mode, contents) = records
            .iter()
            .find(|(name, _, _)| name == "init")
            .expect("init record must be present");
        assert_eq!(name, "init");
        assert_eq!(*mode, S_IFREG | 0o755);
        assert_eq!(contents, b"#!static-init-binary");

        // The root directory record is always present.
        assert!(records.iter().any(|(name, mode, _)| name == "." && *mode == S_IFDIR | 0o755));
    }

    #[test]
    fn build_is_byte_for_byte_reproducible() {
        let entries = vec![init_entry(b"same contents every time")];
        let a = build_reproducible_initramfs(&entries).unwrap();
        let b = build_reproducible_initramfs(&entries).unwrap();
        assert_eq!(a, b, "two builds of the same entries must be byte-identical");
    }

    #[test]
    fn changing_file_contents_changes_the_output() {
        let a = build_reproducible_initramfs(&[init_entry(b"version A")]).unwrap();
        let b = build_reproducible_initramfs(&[init_entry(b"version B")]).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn nested_paths_get_synthesized_directory_entries() {
        let entries = vec![InitramfsEntry::regular("bin/harness", 0o755, b"harness-binary".to_vec())];
        let gz = build_reproducible_initramfs(&entries).unwrap();
        let records = parse_newc_cpio(&gunzip(&gz));

        let names: Vec<&str> = records.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(names, vec![".", "bin", "bin/harness"], "must sort like `find . | sort`");
        assert!(records.iter().any(|(name, mode, _)| name == "bin" && *mode == S_IFDIR | 0o755));
    }

    #[test]
    fn multiple_distinct_files_are_all_preserved() {
        // todo.md §14 item 1's named gap: every existing test/caller of this builder used exactly
        // one entry. A real guest rootfs needs more than `/init` alone (e.g. §11's eventual
        // harness + emulator pair) -- this asserts the builder genuinely archives N>1 distinct
        // files, not just N=1 or N duplicates-that-get-rejected.
        let entries = vec![
            init_entry(b"init-binary-contents"),
            InitramfsEntry::regular("helper", 0o755, b"helper-binary-contents".to_vec()),
            InitramfsEntry::regular("bin/tool", 0o700, b"nested-tool-contents".to_vec()),
        ];
        let gz = build_reproducible_initramfs(&entries).unwrap();
        let records = parse_newc_cpio(&gunzip(&gz));

        let find = |name: &str| {
            records.iter().find(|(n, _, _)| n == name).unwrap_or_else(|| panic!("{name} record must be present"))
        };
        let (_, init_mode, init_contents) = find("init");
        assert_eq!(*init_mode, S_IFREG | 0o755);
        assert_eq!(init_contents, b"init-binary-contents");

        let (_, helper_mode, helper_contents) = find("helper");
        assert_eq!(*helper_mode, S_IFREG | 0o755);
        assert_eq!(helper_contents, b"helper-binary-contents");

        let (_, tool_mode, tool_contents) = find("bin/tool");
        assert_eq!(*tool_mode, S_IFREG | 0o700);
        assert_eq!(tool_contents, b"nested-tool-contents");

        // Exactly the three files plus their two implied directories (".", "bin") -- no entry
        // silently dropped or duplicated.
        assert_eq!(records.len(), 5, "expected 3 files + 2 directories, got: {records:?}");
    }

    #[test]
    fn duplicate_path_is_rejected() {
        let entries = vec![init_entry(b"a"), init_entry(b"b")];
        let err = build_reproducible_initramfs(&entries).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn absolute_path_is_rejected() {
        let entries = vec![InitramfsEntry::regular("/init", 0o755, vec![])];
        let err = build_reproducible_initramfs(&entries).unwrap_err();
        assert!(err.to_string().contains("relative path"), "{err}");
    }

    #[test]
    fn dot_dot_path_is_rejected() {
        let entries = vec![InitramfsEntry::regular("../escape", 0o755, vec![])];
        let err = build_reproducible_initramfs(&entries).unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
    }

    #[test]
    fn round_trip_preserves_a_symlink() {
        // The real-world case this exists for: a glibc rootfs's dynamic linker is almost always
        // reached through a symlink (e.g. `/lib64/ld-linux-x86-64.so.2` ->
        // `../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2` on Debian/Ubuntu) -- todo.md §14 item 1's
        // H8-prerequisite gap ("InitramfsEntry has no symlink entry type").
        let entries = vec![
            InitramfsEntry::regular("lib/x86_64-linux-gnu/ld-linux-x86-64.so.2", 0o755, b"real-loader-bytes".to_vec()),
            InitramfsEntry::symlink("lib64/ld-linux-x86-64.so.2", "../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"),
        ];
        let gz = build_reproducible_initramfs(&entries).unwrap();
        let records = parse_newc_cpio(&gunzip(&gz));

        let (_, link_mode, link_target) = records
            .iter()
            .find(|(name, _, _)| name == "lib64/ld-linux-x86-64.so.2")
            .expect("symlink record must be present");
        assert_eq!(*link_mode, S_IFLNK | SYMLINK_MODE);
        assert_eq!(link_target, b"../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2");

        // Both the symlink's own implied directory ("lib64") and the real file's nested
        // directories ("lib", "lib/x86_64-linux-gnu") must be synthesized, exactly as for a
        // regular file.
        let names: Vec<&str> = records.iter().map(|(name, _, _)| name.as_str()).collect();
        assert!(names.contains(&"lib64"));
        assert!(names.contains(&"lib"));
        assert!(names.contains(&"lib/x86_64-linux-gnu"));
    }

    #[test]
    fn build_is_byte_for_byte_reproducible_with_a_symlink() {
        let entries = vec![InitramfsEntry::symlink("lib64/ld-linux-x86-64.so.2", "../lib/ld.so")];
        let a = build_reproducible_initramfs(&entries).unwrap();
        let b = build_reproducible_initramfs(&entries).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn empty_symlink_target_is_rejected() {
        let entries = vec![InitramfsEntry::symlink("dangling", "")];
        let err = build_reproducible_initramfs(&entries).unwrap_err();
        assert!(err.to_string().contains("empty target"), "{err}");
    }
}
