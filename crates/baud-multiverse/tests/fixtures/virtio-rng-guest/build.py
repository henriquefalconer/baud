#!/usr/bin/env python3
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# SPDX-License-Identifier: Proprietary
#
# Regenerates bzImage in this directory from payload.s. Run from anywhere; writes alongside
# itself. Needs only `as`/`ld`/`objcopy`/`nm` (binutils) -- no kernel source, no Nix, no
# cross-compiler. Identical mechanics to ../timer-guest/build.py (only VECTOR differs, and the
# comment below on why); see that fixture's BUILD.md for the wrapping-header rationale shared by
# every hand-assembled flat-binary fixture in this crate.

import re
import struct
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

SETUP_SECTS = 4
SETUP_SIZE = (SETUP_SECTS + 1) * 512  # 2560 bytes

ENTRY_OFFSET = 0x200  # baud-multiverse always sets RIP = kernel_load + this offset.

# Must match payload.s's own `.equ RUNTIME_BASE`/`.equ VECTOR` exactly.
RUNTIME_BASE = 0x00200200
VECTOR = 0x31  # distinct from ../timer-guest's 0x30, so both fixtures' IDTs never collide if a
# future test ever boots them side by side.


def symbol_addresses(elf_path: Path) -> dict:
    out = subprocess.run(["nm", "--defined-only", str(elf_path)], check=True, capture_output=True, text=True).stdout
    addrs = {}
    for line in out.splitlines():
        m = re.match(r"^([0-9a-fA-F]+)\s+\S\s+(\S+)$", line.strip())
        if m:
            addrs[m.group(2)] = int(m.group(1), 16)
    return addrs


def assemble_payload() -> bytes:
    obj = HERE / "payload.o"
    elf = HERE / "payload.elf"
    flat = HERE / "payload.bin"
    subprocess.run(["as", "--64", "-o", str(obj), str(HERE / "payload.s")], check=True)
    subprocess.run(
        ["ld", "-Ttext=0", "-e", "_start", "-o", str(elf), str(obj)],
        check=True,
    )
    addrs = symbol_addresses(elf)
    subprocess.run(["objcopy", "-O", "binary", str(elf), str(flat)], check=True)
    data = bytearray(flat.read_bytes())

    isr_runtime = addrs["isr"] + RUNTIME_BASE
    gate_offset = addrs["idt_start"] + VECTOR * 16
    struct.pack_into("<H", data, gate_offset + 0, isr_runtime & 0xFFFF)
    struct.pack_into("<H", data, gate_offset + 6, (isr_runtime >> 16) & 0xFFFF)
    struct.pack_into("<I", data, gate_offset + 8, (isr_runtime >> 32) & 0xFFFFFFFF)

    obj.unlink()
    elf.unlink()
    flat.unlink()
    return bytes(data)


def build_header() -> bytearray:
    header = bytearray(SETUP_SIZE)
    header[0x1F1] = SETUP_SECTS
    struct.pack_into("<H", header, 0x1FE, 0xAA55)  # boot_flag
    struct.pack_into("<I", header, 0x202, 0x5372_6448)  # header magic "HdrS"
    struct.pack_into("<H", header, 0x206, 0x0200)  # version >= 2.00
    header[0x211] = 0x01  # loadflags bit 0 (LOADED_HIGH)
    struct.pack_into("<I", header, 0x214, 0x0020_0000)  # code32_start >= HIMEM_START (0x100000)
    return header


def main() -> int:
    payload = assemble_payload()
    header = build_header()
    body = bytearray(ENTRY_OFFSET) + payload
    image = bytes(header) + bytes(body)
    out = HERE / "bzImage"
    out.write_bytes(image)
    print(f"wrote {out} ({len(image)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
