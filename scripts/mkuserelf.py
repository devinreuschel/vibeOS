#!/usr/bin/env python3
"""Wrap a nasm -f bin blob as ELF64 ET_EXEC. Load VA 1 GiB (past identity)."""

from __future__ import annotations

import struct
import sys

LOAD_VA = 0x4000_0000
FILE_OFF = 0x1000
PT_LOAD = 1
PT_GNU_STACK = 0x6474E551
PF_X, PF_W, PF_R = 1, 2, 4
ET_EXEC = 2
EM_X86_64 = 62
EHDR = 64
PHDR = 56


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: mkuserelf.py <blob> <out.elf>", file=sys.stderr)
        return 2
    code = open(sys.argv[1], "rb").read()
    if not code:
        print("empty blob", file=sys.stderr)
        return 1
    phnum = 2
    out = bytearray(FILE_OFF + len(code))
    out[0:4] = b"\x7fELF"
    out[4] = 2
    out[5] = 1
    out[6] = 1
    struct.pack_into("<HHIQQQIHHHHHH", out, 16,
                     ET_EXEC, EM_X86_64, 1, LOAD_VA, EHDR, 0, 0,
                     EHDR, PHDR, phnum, 0, 0, 0)
    # PT_LOAD RX
    struct.pack_into("<IIQQQQQQ", out, EHDR,
                     PT_LOAD, PF_R | PF_X, FILE_OFF, LOAD_VA, LOAD_VA,
                     len(code), len(code), 0x1000)
    # PT_GNU_STACK RW NX
    struct.pack_into("<IIQQQQQQ", out, EHDR + PHDR,
                     PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 0, 16)
    out[FILE_OFF:FILE_OFF + len(code)] = code
    open(sys.argv[2], "wb").write(out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
