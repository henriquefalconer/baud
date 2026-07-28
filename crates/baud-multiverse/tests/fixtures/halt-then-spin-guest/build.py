#!/usr/bin/env python3
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# SPDX-License-Identifier: Proprietary
#
# Regenerates bzImage in this directory from payload.s. Run from anywhere; writes
# alongside itself. Needs only `as`/`ld`/`objcopy`/`nm` (binutils) -- no kernel source, no Nix,
# no cross-compiler. See BUILD.md for why this file's format is what it is and what it contains.
# Wrapping mechanics (header/ENTRY_OFFSET) and the IDT-gate byte-patching below are identical to
# ../timer-guest/build.py (this fixture's direct template) -- only payload.s's actual instructions
# differ (halt once, then spin forever on wake, instead of halting again after each interrupt).

import re
import struct
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

SETUP_SECTS = 4
SETUP_SIZE = (SETUP_SECTS + 1) * 512  # 2560 bytes -- see ../hello-guest/BUILD.md for the full
# Linux/x86 boot-protocol rationale; unchanged here.

ENTRY_OFFSET = 0x200  # baud-multiverse always sets RIP = kernel_load + this offset.

# Must match payload.s's own `.equ RUNTIME_BASE`/`.equ VECTOR` exactly -- the actual
# guest-physical address this flat binary is entered at (layout::KERNEL_LOAD_ADDR +
# layout::KERNEL_64BIT_ENTRY_OFFSET), and the one IDT vector this fixture's gate fills in.
RUNTIME_BASE = 0x00200200
VECTOR = 0x30


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

    # Patch the VECTOR-th IDT gate's three offset fields with `isr`'s real runtime address --
    # GAS/ld resolved every plain-addition expression in payload.s already (e.g. `idtr`'s `.quad
    # (idt_start + RUNTIME_BASE)`); only the bitwise split of `isr`'s address across this gate's
    # 16/16/32-bit fields needed deferring to here. See BUILD.md and payload.s's own comment.
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
