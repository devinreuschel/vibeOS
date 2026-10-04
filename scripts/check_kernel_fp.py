#!/usr/bin/env python3
"""No FP or SIMD instruction in the kernel outside its save and load routines.

The kernel target is soft-float and `CR0.TS` stays clear, so nothing traps a
kernel use of the x87, MMX, SSE, or AVX registers: one would silently
corrupt the state of whichever user thread the registers hold (ROADMAP
§10.6, the FP binding box; DESIGN §7.5). This script disassembles each
kernel ELF it is given with the pinned toolchain's `llvm-objdump` (the
`llvm-tools` component) and fails on any such instruction outside the
routines `ALLOW` names, each for the mnemonics listed there.

    check_kernel_fp.py [--objdump PATH] [ELF ...]

With no ELF it prints `check_kernel_fp: no ELF given` and exits 0, which is
how `make check`'s loop runs it; the Makefile's `KERNEL_VARIANT` runs it on
each linked kernel.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from collections.abc import Iterable
from pathlib import Path

# Demangled symbol -> the FP mnemonics it may use, per ELF machine.
ALLOW_X86: dict[str, frozenset[str]] = {
    "vibeos::proc::syscall_init::fp_save": frozenset({"fxsave64"}),
    "vibeos::proc::syscall_init::fp_load": frozenset({"fxrstor64"}),
}
ALLOW_AARCH64: dict[str, frozenset[str]] = {
    "vibeos::proc::syscall_init::fp_save": frozenset({"stp", "str", "mrs"}),
    "vibeos::proc::syscall_init::fp_load": frozenset({"ldp", "ldr", "msr"}),
}

# Sections that hold no 64-bit kernel code (the AP trampoline is 16- and
# 32-bit code the 64-bit disassembly would misread).
SKIP_SECTIONS = (".trampoline",)

PREFIXES = frozenset({
    "lock", "rep", "repe", "repz", "repne", "repnz", "data16", "data32", "addr32",
    "cs", "ds", "es", "fs", "gs", "ss", "notrack", "bnd",
})

# Mnemonics that touch FP or vector state without naming a vector register.
FP_MNEMONICS = frozenset({
    "emms", "femms", "ldmxcsr", "stmxcsr", "vldmxcsr", "vstmxcsr", "vzeroupper",
    "vzeroall", "wait", "xsave", "xsave64", "xsaveopt", "xsaveopt64", "xsavec",
    "xsavec64", "xsaves", "xsaves64", "xrstor", "xrstor64", "xrstors", "xrstors64",
})

VECTOR_REG = re.compile(r"\b(?:[xyz]mm\d+|mm[0-7]|st(?:\(\d\))?|k[0-7])\b")
VECTOR_A64 = re.compile(r"\b(?:[vqdsbhz]\d+|p\d+|fpcr|fpsr)\b")
EM_AARCH64 = 183
SECTION = re.compile(r"^Disassembly of section (\S+):$")
SYMBOL = re.compile(r"^[0-9a-f]+ <(.*)>:$")
INSN = re.compile(r"^\s*([0-9a-f]+):\s+(\S+)(?:\s+(.*))?$")
HASH = re.compile(r"::h[0-9a-f]{16}$")
LLVM_SUFFIX = re.compile(r"\s*\(\.llvm\.\d+\)$|\.llvm\.\d+$")
ANGLE = re.compile(r"<[^>]*>")


def default_objdump() -> str:
    """`llvm-objdump` of the active toolchain's host `bin` directory."""
    try:
        sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True,
                                 text=True, check=True).stdout.strip()
        vv = subprocess.run(["rustc", "-vV"], capture_output=True, text=True,
                            check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return "llvm-objdump"
    host = next((ln.split(": ", 1)[1] for ln in vv.splitlines() if ln.startswith("host: ")), "")
    return str(Path(sysroot) / "lib" / "rustlib" / host / "bin" / "llvm-objdump")


def symbol_name(raw: str) -> str:
    """`raw` less its `::h<16 hex>` hash and any `.llvm.<n>` suffix."""
    return HASH.sub("", LLVM_SUFFIX.sub("", raw))


def is_fp(mnemonic: str, operands: str, *, aarch64: bool = False) -> bool:
    """Whether the instruction touches FP or vector state."""
    ops = ANGLE.sub("", operands)
    if aarch64:
        return bool(VECTOR_A64.search(ops))
    if mnemonic in FP_MNEMONICS:
        return True
    # Every x87 mnemonic, `fxsave`/`fxrstor` included, starts with `f`,
    # and no other x86 mnemonic does.
    if mnemonic.startswith("f"):
        return True
    return bool(VECTOR_REG.search(ops))


def elf_is_aarch64(path: str) -> bool:
    """Whether `path` is an ELF with `e_machine` `EM_AARCH64`."""
    try:
        with open(path, "rb") as f:
            hdr = f.read(20)
    except OSError:
        return False
    return (
        len(hdr) >= 20
        and hdr[:4] == b"\x7fELF"
        and hdr[18] == (EM_AARCH64 & 0xFF)
        and hdr[19] == 0
    )


def scan(listing: Iterable[str], *, aarch64: bool = False) -> list[str]:
    """Each FP instruction in an `llvm-objdump -d` listing outside `ALLOW`."""
    allow = ALLOW_AARCH64 if aarch64 else ALLOW_X86
    errors: list[str] = []
    section = ""
    sym = ""
    for line in listing:
        line = line.rstrip("\n")
        m = SECTION.match(line)
        if m:
            section = m.group(1)
            sym = ""
            continue
        if section.startswith(SKIP_SECTIONS):
            continue
        m = SYMBOL.match(line)
        if m:
            sym = symbol_name(m.group(1))
            continue
        m = INSN.match(line)
        if not m:
            continue
        addr, rest = m.group(1), f"{m.group(2)} {m.group(3) or ''}".split()
        while rest and rest[0] in PREFIXES:
            rest = rest[1:]
        if not rest:
            continue
        mnemonic, operands = rest[0], " ".join(rest[1:])
        if not is_fp(mnemonic, operands, aarch64=aarch64):
            continue
        if mnemonic in allow.get(sym, frozenset()):
            continue
        errors.append(f"{sym or '?'}: {addr}: {mnemonic} {operands}".rstrip())
    return errors


def disassemble(objdump: str, elf: str) -> list[str]:
    """The disassembly of `elf`; Intel syntax on x86_64."""
    cmd = [objdump, "-d", "--demangle", "--no-show-raw-insn"]
    if not elf_is_aarch64(elf):
        cmd.append("-M")
        cmd.append("intel")
    cmd.append(elf)
    out = subprocess.run(cmd, capture_output=True, text=True, check=True)
    return out.stdout.splitlines()


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="check_kernel_fp.py")
    ap.add_argument("--objdump", default=None)
    ap.add_argument("elf", nargs="*")
    args = ap.parse_args(argv if argv is not None else [])
    if not args.elf:
        print("check_kernel_fp: no ELF given")
        return 0
    objdump = args.objdump or default_objdump()
    failed = False
    for elf in args.elf:
        try:
            listing = disassemble(objdump, elf)
        except (OSError, subprocess.CalledProcessError) as e:
            print(f"check_kernel_fp: {elf}: objdump failed: {e}", file=sys.stderr)
            return 2
        for err in scan(listing, aarch64=elf_is_aarch64(elf)):
            print(f"check_kernel_fp: {elf}: {err}", file=sys.stderr)
            failed = True
    if failed:
        print("check_kernel_fp: FP or SIMD instruction outside the save and load routines "
              "(ROADMAP §10.6, DESIGN §7.5)", file=sys.stderr)
        return 1
    print("check_kernel_fp: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
