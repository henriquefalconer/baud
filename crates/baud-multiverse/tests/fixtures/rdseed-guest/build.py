#!/usr/bin/env python3
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# SPDX-License-Identifier: Proprietary
#
# Regenerates bzImage in this directory from payload.s. Run from anywhere; writes
# alongside itself. Needs only `as`/`ld` (binutils) -- no kernel source, no Nix, no
# cross-compiler. See BUILD.md for why this file's format is what it is and what it contains.
# Identical wrapping mechanics to ../rdrand-guest/build.py and ../hello-guest/build.py
# (same bzImage-header rationale) -- only the assembled payload, plus the one extra
# `rewrite_rdseed` step below, differ.

import struct
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

SETUP_SECTS = 4
SETUP_SIZE = (SETUP_SECTS + 1) * 512  # 2560 bytes -- see ../hello-guest/BUILD.md for the full
# Linux/x86 boot-protocol rationale; unchanged here.

ENTRY_OFFSET = 0x200  # baud-multiverse always sets RIP = kernel_load + this offset.

KERNEL_LOAD_ADDR = 0x0020_0000  # crates/baud-multiverse/src/layout.rs's KERNEL_LOAD_ADDR --
# `linux_loader`'s BzImage loader honours the `kernel_offset` baud passes it verbatim, so this is
# where the post-setup body of this image actually lands in guest physical memory.

RDSEED_EAX = bytes([0x0F, 0xC7, 0xF8])  # `rdseed eax` (0F C7 /7, ModRM reg=eax)
UD2_PLUS_NOP = bytes([0x0F, 0x0B, 0x90])  # exactly what `baud_packages::rewrite_rdseed` writes:
# UD2 (0F 0B), NOP-padded (90) out to the original instruction's encoded length, in place.


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


def rewrite_rdseed(payload: bytes) -> tuple[bytes, int]:
    """The `baud image build` step, replicated: overwrite the one `rdseed eax` encoding in the
    assembled payload with `UD2` + `NOP` padding, in place and length-preserving -- byte-for-byte
    what `baud_packages::rewrite_rdseed` (crates/baud-packages/src/rdseed.rs, todo.md §4) emits
    for this site. Done here rather than by calling that crate because this fixture is a flat
    binary, not the ELF `rewrite_rdseed` parses; the *bytes written* are identical either way,
    which is the only thing the enforced-regime serve path can observe.
    """
    offset = payload.find(RDSEED_EAX)
    if offset < 0:
        raise SystemExit("payload.s no longer contains an `rdseed eax` encoding to rewrite")
    if payload.find(RDSEED_EAX, offset + 1) >= 0:
        raise SystemExit("payload.s contains more than one `rdseed eax` encoding; BUILD.md and "
                         "the Rust test both assume exactly one site")
    return payload[:offset] + UD2_PLUS_NOP + payload[offset + len(RDSEED_EAX):], offset


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
    payload, rdseed_offset = rewrite_rdseed(assemble_payload())
    header = build_header()
    body = bytearray(ENTRY_OFFSET) + payload
    image = bytes(header) + bytes(body)
    out = HERE / "bzImage"
    out.write_bytes(image)
    print(f"wrote {out} ({len(image)} bytes)")
    # The one number the Rust test has to hardcode -- see BUILD.md's "Where the UD2 is".
    print(
        f"  rdseed->UD2 site: payload offset {rdseed_offset:#x}, "
        f"file offset {SETUP_SIZE + ENTRY_OFFSET + rdseed_offset:#x}, "
        f"guest address {KERNEL_LOAD_ADDR + ENTRY_OFFSET + rdseed_offset:#x} "
        f"(gpr_index 0 = RAX/EAX, length 3)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
