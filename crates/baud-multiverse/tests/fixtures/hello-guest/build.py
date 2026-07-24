#!/usr/bin/env python3
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# SPDX-License-Identifier: Proprietary
#
# Regenerates bzImage in this directory from payload.s. Run from anywhere; writes
# alongside itself. Needs only `as`/`ld` (binutils) -- no kernel source, no Nix, no
# cross-compiler. See BUILD.md for why this file's format is what it is and what it contains.

import struct
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

SETUP_SECTS = 4
SETUP_SIZE = (SETUP_SECTS + 1) * 512  # 2560 bytes -- the Linux/x86 boot protocol's real-mode
# header region; BzImage::load derives this same value from setup_sects (defaulting a 0 to 4,
# same arithmetic) to know where the "protected-mode kernel" payload starts in the file.

# The 64-bit entry point every Linux/x86 bzImage is entered at (`layout::KERNEL_64BIT_ENTRY_OFFSET`,
# `startup_64` in a real kernel's arch/x86/boot/compressed/head_64.S) is a *fixed offset from the
# start of the payload*, not from the file start -- baud-multiverse sets RIP to
# `kernel_load + 0x200` unconditionally. The bytes in between are never executed.
ENTRY_OFFSET = 0x200


def assemble_payload() -> bytes:
    """Assemble payload.s into a flat (no ELF headers, no sections) binary blob via the same
    GNU binutils `as`/`ld` already required to build this workspace's KVM-linked crates -- no
    extra toolchain (nasm, a cross-compiler, ...) needed."""
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
    """The minimal Linux/x86 boot-protocol `setup_header` fields `linux_loader::loader::bzimage::
    BzImage::load` actually validates (BUILD.md enumerates each one and why) -- every other byte
    in the header region is left zero, since nothing in this fixture's payload, and nothing in
    `bootparams.rs`'s post-load patching, reads any of the rest."""
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
    # The payload sits ENTRY_OFFSET bytes into the loaded body; everything before it is padding
    # that is loaded into guest RAM but never executed.
    body = bytearray(ENTRY_OFFSET) + payload
    image = bytes(header) + bytes(body)
    out = HERE / "bzImage"
    out.write_bytes(image)
    print(f"wrote {out} ({len(image)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
