// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The build-time `rdseed` -> `UD2`(+`NOP`) rewrite pass (todo.md §3.8, §4, §12 row 15).
//
// Why this exists: the current dev host (WSL2 under Hyper-V L0) cannot hardware-trap `rdseed`
// (its VMX RDSEED-exiting secondary control is masked -- todo.md §3.8's host-capability note),
// so a raw, CPUID-mask-defeating `rdseed` in the guest image would read real host entropy
// straight through. `baud-packages` closes that hole at build time instead: every `rdseed`
// opcode in every executable section of the guest image (kernel and userspace alike) is
// overwritten *in place, length-preserving* with `UD2` + `NOP` padding, so `baud-multiverse` can
// serve the resulting `#UD` a tape-seeded value (its own module, once the VMCS exception-bitmap
// serve path lands -- not this pass's job).
//
// This pass uses a **real x86 decoder** (Capstone), never a byte-grep over `0F C7` -- that
// opcode is a whole *group* (`/6` = RDRAND, `/7` = RDSEED, `/1` = CMPXCHG8B/16B), and a grep
// would both miss context-dependent forms and false-positive on `.rodata`/`.data` bytes that
// merely resemble the pattern. Only sections with `SHF_EXECINSTR` are ever fed to the decoder.

use anyhow::{bail, Context, Result};
use capstone::prelude::*;
use object::{Object, ObjectSection, SectionFlags};
use serde::{Deserialize, Serialize};

/// One `rdseed` opcode located (and, after [`rewrite_rdseed`], patched) in the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdseedSite {
    /// Name of the `SHF_EXECINSTR` section the instruction was decoded from (e.g. `.text`).
    pub section: String,
    /// File offset (byte index into the whole image) of the instruction's first opcode byte.
    pub file_offset: u64,
    /// Virtual address of the instruction, as encoded in the ELF's section header.
    pub address: u64,
    /// Encoded length in bytes: 3 for `RDSEED r32` (`0F C7 /7`), 4 for `RDSEED r64`
    /// (REX.W-prefixed) or `RDSEED r16` (0x66-prefixed) -- whatever Capstone actually decoded,
    /// never assumed.
    pub length: u8,
    /// Destination GPR, in the same 0=RAX..15=R15 numbering `baud-vcpu`'s `gpr_for_index` (and
    /// `EnforcedRdseedSite::gpr_index`) use -- decoded from the instruction's own ModRM byte (`rm`
    /// field, extended by `REX.B` when present), not assumed to be a fixed register. This is what
    /// lets a `RdseedSite` be converted directly into a `baud_vcpu::EnforcedRdseedSite` for
    /// `Multiverse::boot_with_rdseed_sites` (todo.md §14's "`RdseedRewriteReport` -> boot wiring").
    pub gpr_index: u8,
}

/// Decode the destination GPR of a register-direct `RDSEED` encoding (`ModRM.mod == 11`, no
/// SIB/displacement bytes) straight from its raw bytes: the ModRM byte is always the last byte of
/// the instruction, and its `rm` field (low 3 bits), extended by `REX.B` (bit 0 of a `0x40..=0x4F`
/// prefix byte, if the first byte is one) to reach `r8..r15`, names the register -- the exact
/// numbering `gpr_for_index` (`baud-vcpu`) already uses for `RDRAND`'s destination.
fn gpr_index_from_modrm(bytes: &[u8]) -> u8 {
    let modrm = bytes[bytes.len() - 1];
    let rex_b = if (0x40..=0x4F).contains(&bytes[0]) { bytes[0] & 0x1 } else { 0 };
    (modrm & 0x7) | (rex_b << 3)
}

/// Report of a [`rewrite_rdseed`] pass: every site found (and patched).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RdseedRewriteReport {
    pub sites: Vec<RdseedSite>,
}

impl RdseedRewriteReport {
    pub fn count(&self) -> usize {
        self.sites.len()
    }
}

fn new_disassembler() -> Result<Capstone> {
    Capstone::new()
        .x86()
        .mode(arch::x86::ArchMode::Mode64)
        .syntax(arch::x86::ArchSyntax::Intel)
        .detail(true)
        .build()
        .context("failed to build the capstone x86-64 disassembler")
}

/// True if this ELF section carries `SHF_EXECINSTR` -- the only sections the rewrite pass (or
/// its scanner) ever decodes. Non-executable sections (`.rodata`, `.data`, `.debug_*`, ...) are
/// never touched, so byte patterns that merely *resemble* `rdseed` there can never match.
fn is_execinstr<'d, S: ObjectSection<'d>>(section: &S) -> bool {
    matches!(
        section.flags(),
        SectionFlags::Elf { sh_flags } if sh_flags & u64::from(object::elf::SHF_EXECINSTR) != 0
    )
}

/// Decoder-based scan (todo.md's `no_rdseed_opcode_survives_in_image` test): disassemble every
/// `SHF_EXECINSTR` section of `elf_bytes` and report every `rdseed` instruction found. Used both
/// to drive the rewrite and, standalone, to verify a build produced none.
pub fn scan_rdseed_opcodes(elf_bytes: &[u8]) -> Result<Vec<RdseedSite>> {
    let file = object::File::parse(elf_bytes).context("failed to parse guest image as ELF")?;
    let cs = new_disassembler()?;
    let mut sites = Vec::new();

    for section in file.sections() {
        if !is_execinstr(&section) {
            continue;
        }
        let Some((file_off, file_size)) = section.file_range() else {
            continue;
        };
        if file_size == 0 {
            continue;
        }
        let data = &elf_bytes[file_off as usize..(file_off + file_size) as usize];
        let addr = section.address();
        let insns = cs
            .disasm_all(data, addr)
            .context("x86-64 disassembly of an SHF_EXECINSTR section failed")?;
        let name = section.name().unwrap_or("").to_string();
        for insn in insns.iter() {
            if insn
                .mnemonic()
                .is_some_and(|m| m.eq_ignore_ascii_case("rdseed"))
            {
                let insn_off = insn.address() - addr;
                sites.push(RdseedSite {
                    section: name.clone(),
                    file_offset: file_off + insn_off,
                    address: insn.address(),
                    length: insn.bytes().len() as u8,
                    gpr_index: gpr_index_from_modrm(insn.bytes()),
                });
            }
        }
    }

    Ok(sites)
}

/// Apply the build-time rewrite (todo.md §4): every real `rdseed` instruction decoded from a
/// `SHF_EXECINSTR` section is overwritten **in place, length-preserving** --
/// `UD2` (`0F 0B`) followed by `NOP` (`90`) padding out to the original instruction's encoded
/// length. Because no bytes shift, every address, jump target, and relocation in the image stays
/// intact and no relocation/rewriting framework is needed.
///
/// After patching, the pass re-scans (decoder-based, not byte-grep) to confirm zero `rdseed`
/// sites survive, and re-disassembles every touched section to confirm the instruction stream is
/// still well-formed -- both failures are reported as errors, never silently ignored.
pub fn rewrite_rdseed(elf_bytes: &[u8]) -> Result<(Vec<u8>, RdseedRewriteReport)> {
    let sites = scan_rdseed_opcodes(elf_bytes)?;
    let mut patched = elf_bytes.to_vec();

    for site in &sites {
        let off = site.file_offset as usize;
        let len = site.length as usize;
        if len < 2 {
            bail!(
                "rdseed site at file offset {off:#x} decoded with implausible length {len} \
                 (UD2 alone needs 2 bytes)"
            );
        }
        if off + len > patched.len() {
            bail!(
                "rdseed site at file offset {off:#x} (len {len}) runs past the end of the image \
                 ({} bytes)",
                patched.len()
            );
        }
        patched[off] = 0x0F;
        patched[off + 1] = 0x0B;
        for b in &mut patched[off + 2..off + len] {
            *b = 0x90;
        }
    }

    if !sites.is_empty() {
        let residual = scan_rdseed_opcodes(&patched)?;
        if !residual.is_empty() {
            bail!(
                "rdseed rewrite left {} opcode(s) undetected after patching: {residual:?}",
                residual.len()
            );
        }
        // Re-decode every touched section end-to-end to confirm the patched stream is still
        // well-formed (a length-preserving in-place patch cannot desync instruction boundaries,
        // but this is a real re-verification, not an assumption).
        let file = object::File::parse(patched.as_slice())
            .context("failed to re-parse the patched image as ELF")?;
        let cs = new_disassembler()?;
        let touched: std::collections::BTreeSet<&str> =
            sites.iter().map(|s| s.section.as_str()).collect();
        for section in file.sections() {
            let name = section.name().unwrap_or("");
            if !touched.contains(name) {
                continue;
            }
            let Some((file_off, file_size)) = section.file_range() else {
                continue;
            };
            let data = &patched[file_off as usize..(file_off + file_size) as usize];
            cs.disasm_all(data, section.address()).with_context(|| {
                format!("patched section '{name}' no longer decodes as valid x86-64")
            })?;
        }
    }

    Ok((patched, RdseedRewriteReport { sites }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // A minimal hand-built ELF64 fixture: just enough for `object::File::parse` to enumerate
    // sections with real `sh_flags`/`sh_offset`/`sh_addr`/`sh_size` -- no linker, no nix, no
    // cross-toolchain needed to exercise the decoder-based pass.
    // -----------------------------------------------------------------------

    /// Build a minimal ELF64 (`ET_EXEC`, `EM_X86_64`) with one section per `(name, sh_flags,
    /// data)` triple, plus the mandatory NULL section and a `.shstrtab`. Good enough for
    /// `object::File::parse` to read back real section metadata; nothing else about the ELF
    /// (program headers, entry point, symbols) is populated because the rewrite pass never
    /// looks at them.
    fn build_minimal_elf(sections: &[(&str, u64, &[u8])]) -> Vec<u8> {
        const EHDR_SIZE: usize = 64;
        const SHDR_SIZE: usize = 64;

        // Section-header string table: "\0name1\0name2\0..." -- offset 0 is the empty string
        // every ELF section-name-index-0 (the NULL section) points at.
        let mut shstrtab: Vec<u8> = vec![0];
        let mut name_offsets = Vec::with_capacity(sections.len());
        for (name, _, _) in sections {
            name_offsets.push(shstrtab.len() as u32);
            shstrtab.extend_from_slice(name.as_bytes());
            shstrtab.push(0);
        }
        let shstrtab_name_offset = shstrtab.len() as u32;
        shstrtab.extend_from_slice(b".shstrtab");
        shstrtab.push(0);

        // Layout: [ehdr][section data...][shstrtab data][shdrs (8-byte aligned)]
        let mut data_offsets = Vec::with_capacity(sections.len());
        let mut cursor = EHDR_SIZE;
        let mut body = Vec::new();
        for (_, _, data) in sections {
            data_offsets.push(cursor as u64);
            body.extend_from_slice(data);
            cursor += data.len();
        }
        let shstrtab_off = cursor as u64;
        body.extend_from_slice(&shstrtab);
        cursor += shstrtab.len();

        let shdr_start = (cursor + 7) & !7;
        let pad = shdr_start - cursor;
        body.extend(std::iter::repeat_n(0u8, pad));

        let shnum = sections.len() + 2; // NULL + real sections + .shstrtab
        let shstrndx = shnum - 1;

        let mut buf = vec![0u8; EHDR_SIZE];
        buf[0..4].copy_from_slice(b"\x7fELF");
        buf[4] = 2; // ELFCLASS64
        buf[5] = 1; // ELFDATA2LSB
        buf[6] = 1; // EV_CURRENT
        buf[16..18].copy_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
        buf[18..20].copy_from_slice(&62u16.to_le_bytes()); // e_machine = EM_X86_64
        buf[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        buf[40..48].copy_from_slice(&(shdr_start as u64).to_le_bytes()); // e_shoff
        buf[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes()); // e_ehsize
        buf[58..60].copy_from_slice(&(SHDR_SIZE as u16).to_le_bytes()); // e_shentsize
        buf[60..62].copy_from_slice(&(shnum as u16).to_le_bytes()); // e_shnum
        buf[62..64].copy_from_slice(&(shstrndx as u16).to_le_bytes()); // e_shstrndx

        buf.extend_from_slice(&body);
        buf.resize(shdr_start + shnum * SHDR_SIZE, 0);

        // index 0: NULL section header -- already all zero.

        for (i, (_, flags, data)) in sections.iter().enumerate() {
            let sh = shdr_start + (i + 1) * SHDR_SIZE;
            buf[sh..sh + 4].copy_from_slice(&name_offsets[i].to_le_bytes()); // sh_name
            buf[sh + 4..sh + 8].copy_from_slice(&1u32.to_le_bytes()); // sh_type = SHT_PROGBITS
            buf[sh + 8..sh + 16].copy_from_slice(&flags.to_le_bytes()); // sh_flags
            buf[sh + 16..sh + 24].copy_from_slice(&data_offsets[i].to_le_bytes()); // sh_addr (identity-mapped to file offset)
            buf[sh + 24..sh + 32].copy_from_slice(&data_offsets[i].to_le_bytes()); // sh_offset
            buf[sh + 32..sh + 40].copy_from_slice(&(data.len() as u64).to_le_bytes()); // sh_size
            buf[sh + 48..sh + 56].copy_from_slice(&1u64.to_le_bytes()); // sh_addralign
        }

        let shstrtab_idx = sections.len() + 1;
        let sh = shdr_start + shstrtab_idx * SHDR_SIZE;
        buf[sh..sh + 4].copy_from_slice(&shstrtab_name_offset.to_le_bytes());
        buf[sh + 4..sh + 8].copy_from_slice(&3u32.to_le_bytes()); // sh_type = SHT_STRTAB
        buf[sh + 24..sh + 32].copy_from_slice(&shstrtab_off.to_le_bytes());
        buf[sh + 32..sh + 40].copy_from_slice(&(shstrtab.len() as u64).to_le_bytes());
        buf[sh + 48..sh + 56].copy_from_slice(&1u64.to_le_bytes());

        buf
    }

    const SHF_ALLOC: u64 = 0x2;
    const SHF_EXECINSTR: u64 = 0x4;
    const SHF_WRITE: u64 = 0x1;
    const EXEC: u64 = SHF_ALLOC | SHF_EXECINSTR;

    // Hand-assembled x86-64 opcodes (Intel encodings, from the SDM):
    const RDSEED_EAX: [u8; 3] = [0x0F, 0xC7, 0xF8]; // rdseed eax  (ModRM /7, reg=eax)
    const RDSEED_RAX: [u8; 4] = [0x48, 0x0F, 0xC7, 0xF8]; // rdseed rax  (REX.W)
    const RDSEED_ECX: [u8; 3] = [0x0F, 0xC7, 0xF9]; // rdseed ecx  (ModRM /7, rm=ecx)
    const RDSEED_R8D: [u8; 4] = [0x41, 0x0F, 0xC7, 0xF8]; // rdseed r8d  (REX.B, rm=eax|B -> r8d)
    const RDRAND_EAX: [u8; 3] = [0x0F, 0xC7, 0xF0]; // rdrand eax  (ModRM /6, reg=eax)
    const NOP: [u8; 1] = [0x90];

    /// Spec test (todo.md §4/§12 row 15): `image_rewrites_rdseed`. Every `rdseed` opcode in an
    /// executable section is rewritten; RDRAND (a distinct ModRM `/reg` under the same `0F C7`
    /// group opcode) is left completely untouched, and the image length never changes.
    #[test]
    fn image_rewrites_rdseed() {
        let mut code = Vec::new();
        code.extend_from_slice(&RDRAND_EAX);
        code.extend_from_slice(&RDSEED_EAX);
        code.extend_from_slice(&NOP);
        code.extend_from_slice(&RDSEED_RAX);
        code.extend_from_slice(&RDRAND_EAX);

        let elf = build_minimal_elf(&[(".text", EXEC, &code)]);
        let (patched, report) = rewrite_rdseed(&elf).expect("rewrite must succeed");

        assert_eq!(patched.len(), elf.len(), "length-preserving: image size must not change");
        assert_eq!(report.count(), 2, "exactly the two rdseed sites, not the rdrand ones");

        // Every reported site actually starts with UD2 (0F 0B) in the patched image, padded
        // with NOP out to the original instruction length.
        for site in &report.sites {
            let off = site.file_offset as usize;
            let len = site.length as usize;
            assert_eq!(&patched[off..off + 2], &[0x0F, 0x0B], "UD2 at {off:#x}");
            assert!(
                patched[off + 2..off + len].iter().all(|&b| b == 0x90),
                "NOP padding at {off:#x}"
            );
        }

        // Bytes outside the two rdseed encodings (including both rdrand instructions and the
        // lone explicit NOP) are byte-for-byte identical to the original image.
        let rdseed_ranges: Vec<(usize, usize)> = report
            .sites
            .iter()
            .map(|s| (s.file_offset as usize, s.file_offset as usize + s.length as usize))
            .collect();
        for i in 0..elf.len() {
            if rdseed_ranges.iter().any(|&(a, b)| i >= a && i < b) {
                continue;
            }
            assert_eq!(patched[i], elf[i], "byte {i} outside any rdseed site must be untouched");
        }
    }

    /// Spec test (todo.md §4/§12 row 15): `no_rdseed_opcode_survives_in_image`. After the
    /// rewrite, a fresh decoder-based scan of the patched image finds zero `rdseed` opcodes.
    #[test]
    fn no_rdseed_opcode_survives_in_image() {
        let mut code = Vec::new();
        code.extend_from_slice(&RDSEED_EAX);
        code.extend_from_slice(&RDSEED_RAX);
        code.extend_from_slice(&RDRAND_EAX);

        let elf = build_minimal_elf(&[(".text", EXEC, &code)]);
        let (patched, report) = rewrite_rdseed(&elf).unwrap();
        assert_eq!(report.count(), 2);

        let residual = scan_rdseed_opcodes(&patched).unwrap();
        assert!(residual.is_empty(), "no rdseed opcode may survive: {residual:?}");
    }

    /// A guest whose only `rdseed` bytes live in a non-executable section (data that merely
    /// resembles the opcode) is left completely alone -- this is the decoder-based-scan
    /// guarantee (todo.md §4: "never byte-grep") in its most literal form: a byte-grep would
    /// have matched this and corrupted innocent data.
    #[test]
    fn bytes_resembling_rdseed_in_data_section_are_not_touched() {
        let code = RDRAND_EAX.to_vec(); // .text: only a harmless rdrand.
        let data = RDSEED_EAX.to_vec(); // .data: byte-identical to a real rdseed encoding.

        let elf = build_minimal_elf(&[
            (".text", EXEC, &code),
            (".data", SHF_ALLOC | SHF_WRITE, &data),
        ]);
        let (patched, report) = rewrite_rdseed(&elf).unwrap();
        assert_eq!(report.count(), 0, "no SHF_EXECINSTR section contains rdseed");
        assert_eq!(patched, elf, "non-executable section must be byte-for-byte untouched");
    }

    /// A guest image containing no `rdseed` at all round-trips through the pass as a no-op
    /// (todo.md §4: "a no-op here" for images like the Mario emulator that touch no entropy
    /// instruction at all).
    #[test]
    fn image_without_rdseed_is_a_no_op() {
        let code = RDRAND_EAX.to_vec();
        let elf = build_minimal_elf(&[(".text", EXEC, &code)]);
        let (patched, report) = rewrite_rdseed(&elf).unwrap();
        assert_eq!(report.count(), 0);
        assert_eq!(patched, elf);
    }

    /// Spec test (todo.md §14's `RdseedRewriteReport` -> boot wiring): the reported `gpr_index`
    /// names the *actual* destination register of each site -- `eax`/`rax` (index 0), a non-`eax`
    /// 32-bit register (`ecx`, index 1), and a `REX.B`-extended register (`r8d`, index 8) -- not a
    /// value hardcoded to whatever the fixture happens to use, since `Multiverse::boot_with_rdseed_sites`
    /// trusts this field verbatim to pick which guest register to serve a value into.
    #[test]
    fn gpr_index_names_the_real_destination_register() {
        let mut code = Vec::new();
        code.extend_from_slice(&RDSEED_EAX);
        code.extend_from_slice(&RDSEED_ECX);
        code.extend_from_slice(&RDSEED_R8D);
        code.extend_from_slice(&RDSEED_RAX);

        let elf = build_minimal_elf(&[(".text", EXEC, &code)]);
        let sites = scan_rdseed_opcodes(&elf).unwrap();
        assert_eq!(sites.len(), 4);
        assert_eq!(sites[0].gpr_index, 0, "rdseed eax -> RAX");
        assert_eq!(sites[1].gpr_index, 1, "rdseed ecx -> RCX");
        assert_eq!(sites[2].gpr_index, 8, "rdseed r8d -> R8 (REX.B-extended)");
        assert_eq!(sites[3].gpr_index, 0, "rdseed rax -> RAX (REX.W, rm unextended)");
    }

    /// Multiple executable sections (e.g. a kernel image's `.text` plus a userspace binary's own
    /// `.text` concatenated into one image, or `.init`/`.fini`) are all scanned and patched --
    /// not just the first one found.
    #[test]
    fn rdseed_is_rewritten_across_multiple_executable_sections() {
        let text = RDSEED_EAX.to_vec();
        let init = RDSEED_RAX.to_vec();
        let elf = build_minimal_elf(&[(".text", EXEC, &text), (".init", EXEC, &init)]);
        let (patched, report) = rewrite_rdseed(&elf).unwrap();
        assert_eq!(report.count(), 2);
        assert!(report.sites.iter().any(|s| s.section == ".text"));
        assert!(report.sites.iter().any(|s| s.section == ".init"));
        assert!(scan_rdseed_opcodes(&patched).unwrap().is_empty());
    }

    // -------------------------------------------------------------------
    // Property test: any mix of rdrand/rdseed instructions in any order is fully rewritten --
    // every rdseed site removed, every rdrand site preserved byte-for-byte, image length
    // invariant -- regardless of how many of each or what order they appear in.
    // -------------------------------------------------------------------
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn any_mix_of_rdrand_and_rdseed_is_fully_rewritten(
            pattern in prop::collection::vec(any::<bool>(), 1..24)
        ) {
            let mut code = Vec::new();
            let mut expected_rdseed_count = 0usize;
            for is_rdseed in &pattern {
                if *is_rdseed {
                    code.extend_from_slice(&RDSEED_EAX);
                    expected_rdseed_count += 1;
                } else {
                    code.extend_from_slice(&RDRAND_EAX);
                }
            }
            let elf = build_minimal_elf(&[(".text", EXEC, &code)]);
            let (patched, report) = rewrite_rdseed(&elf).unwrap();

            prop_assert_eq!(report.count(), expected_rdseed_count);
            prop_assert_eq!(patched.len(), elf.len());
            prop_assert!(scan_rdseed_opcodes(&patched).unwrap().is_empty());

            // rdrand instructions (untouched) still decode as rdrand at the same offsets.
            let remaining = object::File::parse(patched.as_slice()).unwrap();
            let text = remaining.sections().find(|s| s.name() == Ok(".text")).unwrap();
            let (off, size) = text.file_range().unwrap();
            let data = &patched[off as usize..(off + size) as usize];
            let cs = new_disassembler().unwrap();
            let insns = cs.disasm_all(data, text.address()).unwrap();
            let mnemonics: Vec<&str> = insns.iter().map(|i| i.mnemonic().unwrap()).collect();
            let expected: Vec<&str> = pattern
                .iter()
                .map(|is_rdseed| if *is_rdseed { "ud2" } else { "rdrand" })
                .collect();
            // NOP padding after each ud2 also decodes as its own instruction, so compare only
            // the non-nop mnemonics in order.
            let non_nop: Vec<&str> = mnemonics.into_iter().filter(|m| *m != "nop").collect();
            prop_assert_eq!(non_nop, expected);
        }
    }
}
