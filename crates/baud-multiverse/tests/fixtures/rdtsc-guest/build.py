#!/usr/bin/env python3
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# SPDX-License-Identifier: Proprietary
#
# Regenerates bzImage in this directory from payload.s. Run from anywhere; writes
# alongside itself. Needs only `as`/`ld` (binutils) -- no kernel source, no Nix, no
# cross-compiler. See BUILD.md for why this file's format is what it is and what it contains.
# Identical wrapping mechanics to ../rdrand-guest/build.py -- only the assembled payload differs.

import struct
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

SETUP_SECTS = 4
SETUP_SIZE = (SETUP_SECTS + 1) * 512  # 2560 bytes -- see ../hello-guest/BUILD.md for the full
# Linux/x86 boot-protocol rationale; unchanged here.

ENTRY_OFFSET = 0x200  # baud-multiverse always sets RIP = kernel_load + this offset.


def assemble_payload() -> bytes:
    obj = HERE / "payload.o"
    flat = HERE / "payload.bin"
    subprocess.run(["as", "--64", "-o", str(obj), str(HERE / "payload.s")], check=True)
    subprocess.run(
        ["ld", "--oformat", "binary", "-Ttext=0", "-e", "_start", "-o", str(flat), str(obj)],
        check=True,
    )
    data = flat.read_bytes()
    obj.unlink()
    flat.unlink()
    return data


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
