#!/usr/bin/env python3
"""Each Rust user ELF is a static non-PIE executable the loader takes.

ROADMAP §10.5 builds the user runtime for `x86_64-unknown-linux-musl` and links
it with `rust-lld` as a static `ET_EXEC` below 2 GiB. The loader refuses
static-PIE until §13.10 and maps `PT_LOAD`s page by page, so two segments on
one page lose bytes (F031). A user program built for a soft-float ABI could not
call §14.1's C, which passes `f64` in XMM registers. This script fails an ELF
unless:

  - it is ELF64 little-endian, whole, and of type `ET_EXEC`;
  - every `PT_LOAD` ends at or below 2 GiB;
  - it has no `PT_INTERP` and no `PT_DYNAMIC`;
  - no two `PT_LOAD`s share a 4 KiB page in memory, or in the file;
  - no `PT_LOAD` is both writable and executable;
  - it has a `.symtab` (so run it before the strip), and no symbol in it,
    defined or undefined, is one of compiler-rt's f32 or f64 soft-float
    helpers (`SOFT_FLOAT`).

    check_user_elf.py [ELF ...]

With no ELF it prints `check_user_elf: no ELF given; nothing to check` and
exits 0, which is how `make check`'s loop runs it; `make user` runs it on each
unstripped program.
"""

from __future__ import annotations

import struct
import sys
from collections.abc import Sequence
from pathlib import Path

ET_EXEC = 2
PT_LOAD = 1
PT_DYNAMIC = 2
PT_INTERP = 3
PF_X = 1
PF_W = 2
SHT_SYMTAB = 2
PAGE = 0x1000
LIMIT = 0x8000_0000
EHDR_SIZE = 64
PHDR_SIZE = 56
SHDR_SIZE = 64
SYM_SIZE = 24


def _soft_float() -> frozenset[str]:
    """compiler-rt's f32 (`s`) and f64 (`d`) emulation helpers.

    TI-mode conversions and `__powi{s,d}f2` are left out: hard-float x86_64
    calls them too.
    """
    names: set[str] = set()
    for t in "sd":
        for op in ("add", "sub", "mul", "div"):
            names.add(f"__{op}{t}f3")
        names.add(f"__neg{t}f2")
        for op in ("cmp", "unord", "eq", "ne", "ge", "lt", "le", "gt"):
            names.add(f"__{op}{t}f2")
        for i in "sd":
            names.add(f"__fix{t}f{i}i")
            names.add(f"__fixuns{t}f{i}i")
            names.add(f"__float{i}i{t}f")
            names.add(f"__floatun{i}i{t}f")
    names.add("__extendsfdf2")
    names.add("__truncdfsf2")
    return frozenset(names)


SOFT_FLOAT = _soft_float()


def _page_down(x: int) -> int:
    return x - x % PAGE


def _page_up(x: int) -> int:
    return _page_down(x + PAGE - 1)


def _meet(a: tuple[int, int], b: tuple[int, int]) -> bool:
    return a[0] < b[1] and b[0] < a[1]


def _cstr(data: bytes, off: int) -> str | None:
    if off < 0 or off >= len(data):
        return None
    end = data.find(b"\0", off)
    if end < 0:
        return None
    return data[off:end].decode("latin-1")


def _symbols(data: bytes, shoff: int, shnum: int) -> tuple[list[str], list[str]] | None:
    """The `.symtab` symbol names and any errors, or None when there is none."""
    errs: list[str] = []
    names: list[str] = []
    found = False
    for i in range(shnum):
        off = shoff + i * SHDR_SIZE
        if off + SHDR_SIZE > len(data):
            return names, ["truncated: section header table past end of file"]
        (_, sh_type, _, _, sh_offset, sh_size, sh_link, _, _, sh_entsize) = struct.unpack_from(
            "<IIQQQQIIQQ", data, off
        )
        if sh_type != SHT_SYMTAB:
            continue
        found = True
        if sh_offset + sh_size > len(data) or sh_link >= shnum:
            errs.append("truncated: .symtab past end of file")
            continue
        str_hdr = shoff + sh_link * SHDR_SIZE
        if str_hdr + SHDR_SIZE > len(data):
            errs.append("truncated: .symtab's string table header past end of file")
            continue
        (str_off, str_size) = struct.unpack_from("<QQ", data, str_hdr + 24)
        if str_off + str_size > len(data):
            errs.append("truncated: .strtab past end of file")
            continue
        strtab = data[str_off : str_off + str_size]
        entsize = sh_entsize or SYM_SIZE
        for j in range(sh_size // entsize):
            (st_name,) = struct.unpack_from("<I", data, sh_offset + j * entsize)
            name = _cstr(strtab, st_name)
            if name:
                names.append(name)
    if not found:
        return None
    return names, errs


def check_elf(data: bytes) -> list[str]:
    """The reasons `data` is not an acceptable user ELF; empty when it is."""
    if len(data) < EHDR_SIZE or data[:4] != b"\x7fELF":
        return ["not an ELF file, or truncated"]
    if data[4] != 2 or data[5] != 1:
        return ["not ELF64 little-endian"]
    (e_type, _, _, _, e_phoff, e_shoff, _, _, e_phentsize, e_phnum, e_shentsize, e_shnum, _) = (
        struct.unpack_from("<HHIQQQIHHHHHH", data, 16)
    )
    errs: list[str] = []
    if e_type != ET_EXEC:
        errs.append(f"e_type {e_type}, want ET_EXEC ({ET_EXEC}): static non-PIE only")
    if e_phnum and e_phentsize != PHDR_SIZE:
        return errs + [f"e_phentsize {e_phentsize}, want {PHDR_SIZE}"]
    if e_phoff + e_phnum * PHDR_SIZE > len(data):
        return errs + ["truncated: program header table past end of file"]
    loads: list[tuple[int, int, int, int, int]] = []
    for i in range(e_phnum):
        (p_type, p_flags, p_offset, p_vaddr, _, p_filesz, p_memsz, _) = struct.unpack_from(
            "<IIQQQQQQ", data, e_phoff + i * PHDR_SIZE
        )
        if p_type == PT_INTERP:
            errs.append(f"PT_INTERP (phdr {i}): not a static executable")
        elif p_type == PT_DYNAMIC:
            errs.append(f"PT_DYNAMIC (phdr {i}): not a static executable")
        elif p_type == PT_LOAD:
            end = p_vaddr + p_memsz
            if end > LIMIT:
                errs.append(f"PT_LOAD {i} ends at {end:#x}, past 2 GiB")
            if p_flags & PF_W and p_flags & PF_X:
                errs.append(f"PT_LOAD {i} is writable and executable")
            loads.append((i, p_offset, p_vaddr, p_filesz, p_memsz))
    for n, a in enumerate(loads):
        for b in loads[n + 1 :]:
            am = (_page_down(a[2]), _page_up(a[2] + a[4]))
            bm = (_page_down(b[2]), _page_up(b[2] + b[4]))
            if a[4] > 0 and b[4] > 0 and _meet(am, bm):
                errs.append(f"PT_LOAD {a[0]} and {b[0]} share a page in memory")
            af = (_page_down(a[1]), _page_up(a[1] + a[3]))
            bf = (_page_down(b[1]), _page_up(b[1] + b[3]))
            if a[3] > 0 and b[3] > 0 and _meet(af, bf):
                errs.append(f"PT_LOAD {a[0]} and {b[0]} share a page in the file")
    if e_shnum and e_shentsize != SHDR_SIZE:
        return errs + [f"e_shentsize {e_shentsize}, want {SHDR_SIZE}"]
    syms = _symbols(data, e_shoff, e_shnum)
    if syms is None:
        errs.append("no .symtab: run before strip")
        return errs
    names, sym_errs = syms
    errs += sym_errs
    for name in sorted(set(names) & SOFT_FLOAT):
        errs.append(f"soft-float helper {name}: build for the hard-float user triple")
    return errs


def main(argv: Sequence[str]) -> int:
    if not argv:
        print("check_user_elf: no ELF given; nothing to check")
        return 0
    bad = 0
    for path in argv:
        try:
            data = Path(path).read_bytes()
        except OSError as e:
            print(f"check_user_elf: {path}: {e.strerror}")
            bad += 1
            continue
        errs = check_elf(data)
        for reason in errs:
            print(f"check_user_elf: {path}: {reason}")
        bad += bool(errs)
    if bad:
        return 1
    print(f"check_user_elf: ok ({len(argv)} ELF)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
