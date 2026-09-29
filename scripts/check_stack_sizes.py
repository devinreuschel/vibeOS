#!/usr/bin/env python3
"""No function reachable from a syscall or a shell command has a stack frame
over `BOUND_BYTES` (ROADMAP §10.2, F058; DESIGN §3.5).

Interrupts land on the interrupted thread's kernel stack (DESIGN §2.2) and
syscall bodies take them (DESIGN §2.9), so one frame over the bound leaves
too little of a 16 KiB stack for the paths above and below it and a hard-IRQ
top half on top. This is a screen for one oversized frame; the in-guest
stack measurement (TESTING §8.2) checks whole paths against DESIGN §4.5's
budget.

The kernel is built with `-Z emit-stack-sizes`, and `linker.ld` keeps the
`.stack_sizes` section (non-alloc): per function, its address as 8 bytes and
its frame size as ULEB128. This script reads that section and the symbol
addresses from the default kernel ELF with the standard library, and the
call graph from `llvm-objdump -d` (the `llvm-tools` component):

- The roots are `SYSCALL_ROOTS` and every address-taken function: one whose
  address appears as a code operand other than a direct call or jump
  target, or as an aligned 8-byte word in allocated data other than the
  ksyms table, which names every function for backtraces. An address a
  `linker.ld` symbol also names (`__text_start` is the first function's) is
  not taken by being loaded, and `NOT_ROOTS` names the boot's continuation,
  which runs before any syscall. That covers the
  shell commands through their registry, the table syscalls, `dyn
  InodeOps` vtables, and thread and IRQ entries.
- From the roots, direct `call` edges and jumps into another function (tail
  calls) are closed over. An indirect call is covered by the rule above:
  every function whose address is taken is a root.
- A reachable function with no entry (precompiled `core`, `alloc` and
  `compiler_builtins`, assembly) is listed by `--report`, never failed.
  An entry whose address is not a function start (a function the link
  discarded) is skipped.

    check_stack_sizes.py [--elf PATH] [--objdump PATH] [--report N]

Prints `check_stack_sizes: ok (...)`, or one line per frame over the bound,
naming the function, its frame and a call chain from a root, and exits 1. A
missing ELF, section or root fails too.
"""

from __future__ import annotations

import argparse
import bisect
import re
import struct
import subprocess
import sys
from collections import deque
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The largest frame a function reachable from a syscall or a shell command
# may have (BOOT.md §3.5 states it and why).
BOUND_BYTES = 7168

# Reachable functions whose frame is over the bound today, each with the
# frame it may not grow past and what shrinks it. Kept only while the frame
# is over the bound: an entry whose function is gone or back under the
# bound fails, so the list only shrinks. ROADMAP §10.2's frame-screen box
# ticks when it is empty (BOOT.md §3.5).
KNOWN_OVER: dict[str, tuple[int, str]] = {
    "vibeos::block::cache::cached_read::<vibeos::fs::kernfs::tmpfs::SliceBack, 4>": (
        8424, "the tmpfs block-cache instance's page buffers on the read path"),
    "vibeos::block::cache::cached_write::<vibeos::fs::kernfs::tmpfs::SliceBack, 4>": (
        8360, "the tmpfs block-cache instance's page buffers on the write path"),
}

# Entry points no data or code operand names: the `syscall` instruction's
# target, set through an MSR.
SYSCALL_ROOTS: tuple[str, ...] = ("vibeos_syscall_entry",)

# Address-taken functions that are no root: the boot's continuation, which
# runs once on the bootstrap thread's 64 KiB stack before any syscall or
# shell command (BOOT.md §3.5). A name the ELF lacks fails.
NOT_ROOTS: tuple[str, ...] = ("vibeos::boot_rest",)

# The default variant's ELF (C-BUILD-OUTPUTS); `make check` builds it first.
DEFAULT_ELF = ROOT / "build/kernels/vibeos-default.elf"

SECTION = ".stack_sizes"

# Allocated sections whose words are addresses nothing calls through: the
# ksyms table names every function for panic backtraces (DESIGN §2.5).
NOT_CALLED = frozenset({".ksyms"})

LINKER_SCRIPT = ROOT / "linker.ld"

SHF_ALLOC = 0x2
SHF_EXECINSTR = 0x4
SHT_NOBITS = 8
SHT_SYMTAB = 2
STT_NOTYPE = 0


def uleb128(data: bytes, at: int) -> tuple[int, int]:
    """The ULEB128 value at `data[at:]` and the offset after it."""
    value = shift = 0
    while True:
        if at >= len(data):
            raise ValueError("truncated ULEB128")
        b = data[at]
        at += 1
        value |= (b & 0x7F) << shift
        shift += 7
        if b < 0x80:
            return value, at


def parse_stack_sizes(data: bytes) -> dict[int, int]:
    """`.stack_sizes` bytes as {function address: frame bytes}; a repeated
    address keeps its largest frame."""
    out: dict[int, int] = {}
    at = 0
    while at < len(data):
        if at + 8 > len(data):
            raise ValueError("truncated .stack_sizes entry")
        (addr,) = struct.unpack_from("<Q", data, at)
        size, at = uleb128(data, at + 8)
        out[addr] = max(size, out.get(addr, 0))
    return out


@dataclass(frozen=True)
class Section:
    name: str
    flags: int
    type: int
    addr: int
    data: bytes


def elf_sections(blob: bytes) -> list[Section]:
    """The sections of a little-endian ELF64 file."""
    if blob[:4] != b"\x7fELF" or blob[4] != 2 or blob[5] != 1:
        raise ValueError("not a little-endian ELF64 file")
    shoff, = struct.unpack_from("<Q", blob, 0x28)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", blob, 0x3A)
    raw = []
    for i in range(shnum):
        off = shoff + i * shentsize
        name, typ, flags, addr, offset, size = struct.unpack_from("<IIQQQQ", blob, off)
        body = b"" if typ == SHT_NOBITS else blob[offset:offset + size]
        raw.append((name, typ, flags, addr, body))
    strtab = raw[shstrndx][4] if shstrndx < len(raw) else b""

    def name_at(i: int) -> str:
        end = strtab.find(b"\0", i)
        return strtab[i:end if end >= 0 else len(strtab)].decode("utf-8", "replace")

    return [Section(name_at(n), f, t, a, b) for n, t, f, a, b in raw]


def notype_symbols(blob: bytes) -> dict[str, int]:
    """The ELF's defined `STT_NOTYPE` symbols, name to value: the ones a
    linker script assigns, and assembly labels."""
    shoff, = struct.unpack_from("<Q", blob, 0x28)
    shentsize, shnum, _ = struct.unpack_from("<HHH", blob, 0x3A)
    hdrs = [struct.unpack_from("<IIQQQQIIQQ", blob, shoff + i * shentsize) for i in range(shnum)]
    out: dict[str, int] = {}
    for h in hdrs:
        if h[1] != SHT_SYMTAB or h[6] >= len(hdrs):
            continue
        str_off, str_size = hdrs[h[6]][4], hdrs[h[6]][5]
        strtab = blob[str_off:str_off + str_size]
        for off in range(h[4], h[4] + h[5] - 23, 24):
            name, info, _, shndx, value = struct.unpack_from("<IBBHQ", blob, off)
            if info & 0xF != STT_NOTYPE or shndx == 0 or name == 0:
                continue
            end = strtab.find(b"\0", name)
            out[strtab[name:end].decode("utf-8", "replace")] = value
    return out


_ASSIGN = re.compile(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=", re.M)


def linker_bounds(script: str, symbols: Mapping[str, int]) -> set[int]:
    """Addresses of the symbols the linker script `script` assigns, such as
    `__text_start`: code that loads one names the function laid out there,
    which calls nothing through it."""
    return {symbols[n] for n in _ASSIGN.findall(script) if n in symbols}


@dataclass
class Disasm:
    """What `parse_objdump` reads: function starts and names, direct call
    and jump edges between functions, and the function addresses code
    names as operands."""

    names: dict[int, str]
    edges: dict[int, set[int]]
    code_refs: set[int]


_LABEL = re.compile(r"^([0-9a-f]+) <(.*)>:$")
_INSN = re.compile(r"^\s*([0-9a-f]+):\s+(\S+)\s*(.*)$")
_BRANCH_TARGET = re.compile(r"^(?:\S+\s+)?0x([0-9a-f]+)\b")
_HEX = re.compile(r"(-?)0x([0-9a-f]{8,16})\b")


def parse_objdump(text: str) -> Disasm:
    """`llvm-objdump -d --no-show-raw-insn` output as a `Disasm`. A call or
    jump whose target lies in another function is an edge to the function
    that holds it; any other operand that is a function's address is a
    code reference."""
    names: dict[int, str] = {}
    insns: list[tuple[int, int, str, str]] = []  # (fn, addr, mnemonic, operands)
    cur: int | None = None
    for line in text.splitlines():
        m = _LABEL.match(line)
        if m:
            cur = int(m.group(1), 16)
            names[cur] = m.group(2)
            continue
        m = _INSN.match(line)
        if m and cur is not None:
            insns.append((cur, int(m.group(1), 16), m.group(2), m.group(3)))
    starts = sorted(names)
    edges: dict[int, set[int]] = {a: set() for a in starts}
    code_refs: set[int] = set()

    def holder(addr: int) -> int | None:
        i = bisect.bisect_right(starts, addr) - 1
        return starts[i] if i >= 0 else None

    for fn, _addr, mnem, ops in insns:
        if mnem.startswith(("call", "j")):
            m = _BRANCH_TARGET.match(ops)
            if m:
                tgt = holder(int(m.group(1), 16))
                if tgt is not None and (tgt != fn or mnem.startswith("call")):
                    edges[fn].add(tgt)
                continue
        for neg, h in _HEX.findall(ops):
            # The kernel code model's sign-extended 32-bit immediates print
            # negative: `$-0x7ffe0000` is 0xffffffff80020000.
            v = (1 << 64) - int(h, 16) if neg else int(h, 16)
            if v in names:
                code_refs.add(v)
    return Disasm(names, edges, code_refs)


def address_taken(
    names: Mapping[int, str], code_refs: Iterable[int], sections: Iterable[Section]
) -> set[int]:
    """Function addresses named by code operands or by an aligned 8-byte
    word of an allocated, non-executable section other than `NOT_CALLED`."""
    taken = {a for a in code_refs if a in names}
    for s in sections:
        if not s.flags & SHF_ALLOC or s.flags & SHF_EXECINSTR or s.type == SHT_NOBITS:
            continue
        if s.name in NOT_CALLED:
            continue
        skew = (-s.addr) % 8
        for off in range(skew, len(s.data) - 7, 8):
            (v,) = struct.unpack_from("<Q", s.data, off)
            if v in names:
                taken.add(v)
    return taken


def reachable(roots: Iterable[int], edges: Mapping[int, Iterable[int]]) -> dict[int, int | None]:
    """Every function reachable from `roots`, each with the function it was
    first reached from (`None` for a root)."""
    parent: dict[int, int | None] = {}
    todo: deque[int] = deque()
    for r in roots:
        if r not in parent:
            parent[r] = None
            todo.append(r)
    while todo:
        f = todo.popleft()
        for g in edges.get(f, ()):
            if g not in parent:
                parent[g] = f
                todo.append(g)
    return parent


def chain(parent: Mapping[int, int | None], f: int) -> list[int]:
    """The call chain from a root down to `f`."""
    out = [f]
    while (p := parent.get(out[-1])) is not None:
        out.append(p)
    return out[::-1]


@dataclass(frozen=True)
class Violation:
    addr: int
    frame: int
    chain: tuple[int, ...]


_LLVM_SUFFIX = re.compile(r" \(\.llvm\.\d+\)$")


def base_name(name: str) -> str:
    """A demangled name without ThinLTO's ` (.llvm.<hash>)` suffix."""
    return _LLVM_SUFFIX.sub("", name)


def known_over(
    bad: Iterable[Violation], names: Mapping[int, str], known: Mapping[str, tuple[int, str]]
) -> tuple[list[Violation], list[str]]:
    """Split `bad` by `known`: the violations it does not cover (a function
    not listed, or one over its recorded frame), and why each stale entry
    fails (its function is not over the bound)."""
    left = []
    seen: set[str] = set()
    for v in bad:
        n = base_name(names[v.addr])
        k = known.get(n)
        if k is not None and v.frame <= k[0]:
            seen.add(n)
        else:
            left.append(v)
    stale = [f"KNOWN_OVER entry {n!r} is not over the bound: delete it"
             for n in known if n not in seen and not any(
                 base_name(names[v.addr]) == n for v in left)]
    return left, stale


def violations(
    sizes: Mapping[int, int], parent: Mapping[int, int | None], bound: int
) -> list[Violation]:
    """Reachable functions whose frame is over `bound`, largest first."""
    out = [
        Violation(f, sizes[f], tuple(chain(parent, f)))
        for f in parent
        if sizes.get(f, 0) > bound
    ]
    return sorted(out, key=lambda v: (-v.frame, v.addr))


def default_objdump() -> str:
    """The pinned toolchain's `llvm-objdump`, as the Makefile finds it."""
    try:
        sysroot = subprocess.run(
            ["rustc", "--print", "sysroot"], capture_output=True, text=True, check=True
        ).stdout.strip()
        vv = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return "llvm-objdump"
    host = next((ln.split(": ", 1)[1] for ln in vv.splitlines() if ln.startswith("host: ")), "")
    tool = Path(sysroot) / "lib/rustlib" / host / "bin/llvm-objdump"
    return str(tool) if tool.exists() else "llvm-objdump"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--elf", type=Path, default=DEFAULT_ELF)
    ap.add_argument("--objdump", default=None)
    ap.add_argument("--report", type=int, default=0, metavar="N",
                    help="print the N largest reachable frames and the reachable functions "
                         "with no entry")
    args = ap.parse_args(argv)

    def fail(msg: str) -> int:
        print(f"check_stack_sizes: {msg}", file=sys.stderr)
        return 1

    if not args.elf.is_file():
        return fail(f"{args.elf}: no kernel ELF (make kernel builds it)")
    blob = args.elf.read_bytes()
    try:
        sections = elf_sections(blob)
    except (ValueError, struct.error) as e:
        return fail(f"{args.elf}: {e}")
    sec = next((s for s in sections if s.name == SECTION), None)
    if sec is None:
        return fail(f"{args.elf}: no {SECTION} section (-Z emit-stack-sizes, linker.ld)")
    if sec.flags & SHF_ALLOC:
        return fail(f"{args.elf}: {SECTION} is allocated; linker.ld must keep it INFO")
    try:
        sizes_raw = parse_stack_sizes(sec.data)
    except ValueError as e:
        return fail(f"{args.elf}: {SECTION}: {e}")
    objdump = args.objdump or default_objdump()
    try:
        text = subprocess.run(
            [objdump, "-d", "--no-show-raw-insn", "-C", str(args.elf)],
            capture_output=True, text=True, check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as e:
        return fail(f"{objdump}: {e}")
    dis = parse_objdump(text)
    by_name = {n: a for a, n in dis.names.items()}
    missing = [r for r in SYSCALL_ROOTS if r not in by_name]
    if missing:
        return fail(f"{args.elf}: no root {', '.join(missing)}")
    gone = [r for r in NOT_ROOTS if r not in by_name]
    if gone:
        return fail(f"{args.elf}: no function {', '.join(gone)} (NOT_ROOTS)")
    sizes = {a: s for a, s in sizes_raw.items() if a in dis.names}
    try:
        script = LINKER_SCRIPT.read_text(encoding="utf-8")
        bounds = linker_bounds(script, notype_symbols(blob))
    except (OSError, struct.error) as e:
        return fail(f"{e}")
    roots = [by_name[r] for r in SYSCALL_ROOTS]
    not_roots = bounds | {by_name[n] for n in NOT_ROOTS}
    roots += sorted(address_taken(dis.names, dis.code_refs, sections) - not_roots)
    parent = reachable(roots, dis.edges)
    bad, stale = known_over(violations(sizes, parent, BOUND_BYTES), dis.names, KNOWN_OVER)

    if args.report:
        ranked = sorted((a for a in parent if a in sizes), key=lambda a: -sizes[a])
        print(f"check_stack_sizes: {args.report} largest reachable frames:")
        for a in ranked[:args.report]:
            print(f"  {sizes[a]:6d} {dis.names[a]}")
        none = sorted(dis.names[a] for a in parent if a not in sizes)
        print(f"check_stack_sizes: {len(none)} reachable functions with no entry:")
        for n in none:
            print(f"  {n}")

    for v in bad:
        path = " -> ".join(dis.names[a] for a in v.chain)
        print(f"check_stack_sizes: {dis.names[v.addr]}: frame {v.frame} bytes over "
              f"{BOUND_BYTES}; reached by {path}", file=sys.stderr)
    for why in stale:
        print(f"check_stack_sizes: {why}", file=sys.stderr)
    if bad or stale:
        print(f"check_stack_sizes: a function reachable from a syscall or a shell command "
              f"has a frame over {BOUND_BYTES} bytes (ROADMAP §10.2, BOOT.md §3.5)",
              file=sys.stderr)
        return 1
    top = max((a for a in parent if a in sizes), key=lambda a: sizes[a], default=None)
    largest = f"largest {sizes[top]} {dis.names[top]}" if top is not None else "no entries"
    print(f"check_stack_sizes: ok ({len(parent)} reachable functions, {largest}; "
          f"bound {BOUND_BYTES} bytes, {len(KNOWN_OVER)} known over it)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
