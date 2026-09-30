#!/usr/bin/env python3
"""The shared-page ELF fixtures (ROADMAP §10.6, F031).

Two static x86_64 `ET_EXEC` images whose two `PT_LOAD` segments share a page,
as a binary linked with no page alignment between `.text` and `.data` has
them. With `B = 0x4000_0000`, each file is `0x1018` bytes: the ELF header and
three program headers at offset 0; a `PT_LOAD` R+X at offset 0, vaddr `B`,
`0x1010` bytes; a `PT_LOAD` R+W at offset `0x1010`, vaddr `B + 0x1010`,
8 bytes (both aligned `0x1000`); and a `PT_GNU_STACK` R+W, without which an
older kernel's READ_IMPLIES_EXEC would make the RW page executable. The
entry point is `B + 0x100`, in the page before the shared one.

- `shared_page.elf` compares the 8-byte constant at `B + 0x1000`, the RX
  segment's tail in the shared page, and the value at `B + 0x1010`, the RW
  segment's, then exits 0, or 1 on a mismatch.
- `shared_page_jump.elf` holds `xor edi, edi; mov eax, 60; syscall` at
  `B + 0x1000`, padded with `int3`; its entry sets `edi` to 3 and jumps
  there, which ends in `SIGSEGV` when the shared page is not executable.

The in-guest tests `elf_shared_page` and `elf_shared_page_jump` load the
checked-in files; CI runs them natively on x86_64 Linux, which must give the
same two outcomes (exit 0, `SIGSEGV`).

Usage:
    mkelf_shared.py --write DIR       write both files into DIR
    mkelf_shared.py --check DIR       exit 1 if DIR's files differ from build()
    mkelf_shared.py --run-native DIR  on Linux x86_64 only (else exit 2): check
                                      the files, run both, and require exit 0
                                      and SIGSEGV
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

BASE = 0x4000_0000
FILE_LEN = 0x1018
ENTRY_OFF = 0x100
SHARED_OFF = 0x1000
DATA_OFF = 0x1010
DATA_LEN = 8
PAGE = 0x1000

# The RX segment's tail in the shared page, and the RW segment's value.
CONST = 0x5245_4853_5F58_525F  # "_RX_SHER" little-endian
DATA = 0x4154_4144_5F57_525F  # "_RW_DATA" little-endian

NAMES = {False: "shared_page.elf", True: "shared_page_jump.elf"}

# ELF constants (the System V gABI and the x86-64 psABI).
ET_EXEC = 2
EM_X86_64 = 62
PT_LOAD = 1
PT_GNU_STACK = 0x6474_E551
PF_X = 1
PF_W = 2
PF_R = 4
EHDR_SIZE = 64
PHDR_SIZE = 56

# x86-64 encodings.
SYSCALL = b"\x0f\x05"
MOV_EAX_60 = b"\xb8" + struct.pack("<I", 60)
XOR_EDI_EDI = b"\x31\xff"
INT3 = b"\xcc"


def mov_edi(v: int) -> bytes:
    return b"\xbf" + struct.pack("<I", v)


def mov_rax_abs(addr: int) -> bytes:
    """`mov rax, [addr]` with a 32-bit absolute address."""
    return b"\x48\x8b\x04\x25" + struct.pack("<I", addr)


def movabs_rcx(v: int) -> bytes:
    return b"\x48\xb9" + struct.pack("<Q", v)


CMP_RAX_RCX = b"\x48\x39\xc8"


def exit_with(code: int) -> bytes:
    return (XOR_EDI_EDI if code == 0 else mov_edi(code)) + MOV_EAX_60 + SYSCALL


def entry_code(jump: bool) -> bytes:
    """The code at the entry point, `B + ENTRY_OFF`."""
    if jump:
        # mov edi, 3; mov eax, B + SHARED_OFF; jmp rax
        return mov_edi(3) + b"\xb8" + struct.pack("<I", BASE + SHARED_OFF) + b"\xff\xe0"
    fail = exit_with(1)
    ok = exit_with(0)
    # Each check jumps to `fail` on a mismatch; `ok` ends the passing run.
    second = mov_rax_abs(BASE + DATA_OFF) + movabs_rcx(DATA) + CMP_RAX_RCX
    tail = second + b"\x75" + bytes([len(ok)]) + ok + fail
    first = mov_rax_abs(BASE + SHARED_OFF) + movabs_rcx(CONST) + CMP_RAX_RCX
    return first + b"\x75" + bytes([len(tail) - len(fail)]) + tail


def ehdr(phnum: int) -> bytes:
    ident = b"\x7fELF" + bytes([2, 1, 1, 0]) + bytes(8)
    return ident + struct.pack(
        "<HHIQQQIHHHHHH",
        ET_EXEC,
        EM_X86_64,
        1,
        BASE + ENTRY_OFF,
        EHDR_SIZE,
        0,
        0,
        EHDR_SIZE,
        PHDR_SIZE,
        phnum,
        0,
        0,
        0,
    )


def phdr(ty: int, flags: int, off: int, vaddr: int, size: int, align: int) -> bytes:
    return struct.pack("<IIQQQQQQ", ty, flags, off, vaddr, vaddr, size, size, align)


def build(jump: bool) -> bytes:
    """The bytes of `shared_page_jump.elf` when `jump`, else `shared_page.elf`."""
    out = bytearray(FILE_LEN)
    head = ehdr(3)
    head += phdr(PT_LOAD, PF_R | PF_X, 0, BASE, DATA_OFF, PAGE)
    head += phdr(PT_LOAD, PF_R | PF_W, DATA_OFF, BASE + DATA_OFF, DATA_LEN, PAGE)
    head += phdr(PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 16)
    out[: len(head)] = head
    code = entry_code(jump)
    out[ENTRY_OFF : ENTRY_OFF + len(code)] = code
    if jump:
        shared = exit_with(0)
        out[SHARED_OFF:DATA_OFF] = shared + INT3 * (DATA_OFF - SHARED_OFF - len(shared))
    else:
        out[SHARED_OFF:DATA_OFF] = struct.pack("<Q", CONST) + bytes(8)
    out[DATA_OFF:FILE_LEN] = struct.pack("<Q", DATA)
    return bytes(out)


def write(d: Path) -> int:
    d.mkdir(parents=True, exist_ok=True)
    for jump, name in NAMES.items():
        (d / name).write_bytes(build(jump))
    return 0


def check(d: Path) -> int:
    bad = 0
    for jump, name in NAMES.items():
        p = d / name
        if not p.is_file() or p.read_bytes() != build(jump):
            print(f"mkelf_shared: {p} differs from build(); rerun --write", file=sys.stderr)
            bad = 1
    return bad


def run_native(d: Path) -> int:
    if sys.platform != "linux" or platform.machine() != "x86_64":
        print(
            "mkelf_shared: --run-native needs Linux x86_64, not "
            f"{sys.platform} {platform.machine()}",
            file=sys.stderr,
        )
        return 2
    if check(d):
        return 1
    want = {False: 0, True: -signal.SIGSEGV}
    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        for jump, name in NAMES.items():
            exe = Path(tmp) / name
            shutil.copyfile(d / name, exe)
            os.chmod(exe, 0o755)
            rc = subprocess.run([str(exe)], check=False, timeout=30).returncode
            ok = rc == want[jump]
            verdict = "ok" if ok else "FAIL"
            print(f"mkelf_shared: {name}: returncode {rc}, want {want[jump]}: {verdict}")
            bad |= not ok
    return int(bad)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--write", type=Path, metavar="DIR")
    g.add_argument("--check", type=Path, metavar="DIR")
    g.add_argument("--run-native", type=Path, metavar="DIR")
    a = ap.parse_args(argv)
    if a.write is not None:
        return write(a.write)
    if a.check is not None:
        return check(a.check)
    return run_native(a.run_native)


if __name__ == "__main__":
    sys.exit(main())
