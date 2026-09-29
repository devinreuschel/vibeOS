#!/usr/bin/env python3
"""The user runtime names an architecture only under `user/src/arch/`.

ROADMAP §10.5: nothing in the `vibeos-user` crate names an architecture
outside one `arch` module, so Phase 11 builds it for aarch64 by adding a
directory. This script reads the crate's portable files (`user/src/**/*.rs`,
`user/mem/**/*.rs`, `user/Cargo.toml` and `user/mem/Cargo.toml`, minus
`user/src/arch/**`) and fails on any line, comments included, that holds:

  - `target_arch`;
  - `asm!`, `global_asm!`, `naked_asm!`, or a `naked` attribute;
  - `core::arch` or `std::arch`;
  - an architecture's name (`ARCH_WORDS`);
  - an x86 register name (`REGISTERS`).

`arm` is not a rule (a match arm), nor are `x0`-style names (common
identifiers). The assembly programs `user/*.asm` and `user/sys.inc` are not
the crate.

    check_user_arch.py [--root DIR]

It prints `<path>:<line>: names an architecture outside user/src/arch/:
<match>` per hit and exits 1, or prints `check_user_arch: ok (<n> files)`.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Sequence
from pathlib import Path

ARCH_WORDS = ("x86_64", "x86", "i386", "i686", "amd64", "aarch64", "arm64", "riscv32", "riscv64")
REGISTERS = ("rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "rip") + tuple(
    f"r{n}" for n in range(8, 16)
)

RULES: tuple[re.Pattern[str], ...] = (
    re.compile(r"target_arch"),
    re.compile(r"\b(?:global_asm|naked_asm|asm)!"),
    re.compile(r"#!?\[\s*(?:unsafe\s*\(\s*)?naked\b"),
    re.compile(r"\b(?:core|std)::arch\b"),
    re.compile(r"(?<![A-Za-z0-9])(?:" + "|".join(ARCH_WORDS) + r")(?![A-Za-z0-9])"),
    re.compile(r"\b(?:" + "|".join(REGISTERS) + r")\b"),
)


def crate_files(root: Path) -> list[Path]:
    """The crate's portable files under `root`, sorted."""
    user = root / "user"
    arch = user / "src" / "arch"
    files: set[Path] = set()
    for base in (user / "src", user / "mem"):
        if base.is_dir():
            files.update(p for p in base.rglob("*.rs") if p.is_file())
    for manifest in (user / "Cargo.toml", user / "mem" / "Cargo.toml"):
        if manifest.is_file():
            files.add(manifest)
    return sorted(p for p in files if arch not in p.parents)


def scan_text(text: str) -> list[tuple[int, str]]:
    """(line number, matched text) for each line of `text` that names an
    architecture; one hit per line."""
    hits: list[tuple[int, str]] = []
    for n, line in enumerate(text.splitlines(), 1):
        for rule in RULES:
            m = rule.search(line)
            if m:
                hits.append((n, m.group(0)))
                break
    return hits


def check(root: Path) -> tuple[list[str], int]:
    """The failure lines and the number of files read."""
    errs: list[str] = []
    files = crate_files(root)
    for path in files:
        rel = path.relative_to(root).as_posix()
        for n, what in scan_text(path.read_text(encoding="utf-8", errors="replace")):
            errs.append(f"{rel}:{n}: names an architecture outside user/src/arch/: {what}")
    return errs, len(files)


def main(argv: Sequence[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0] if __doc__ else None)
    ap.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    args = ap.parse_args(argv)
    errs, n = check(args.root)
    for e in errs:
        print(e)
    if errs:
        return 1
    print(f"check_user_arch: ok ({n} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
