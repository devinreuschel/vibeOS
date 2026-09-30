#!/usr/bin/env python3
"""`PerCpu.current` is read only under `src/arch/`.

ROADMAP §10.3 (F039, DESIGN §2.9 rule 5): the running thread's `current`
is read in one instruction that preemption cannot split,
`arch::current_tcb()`, a `gs`-relative load in `src/arch/x86_64/percpu.rs`.
A read through `&PerCpu` takes two steps, the area's address and then the
field, and a thread moved between them reads another CPU's `current`.

This script reads `src/**/*.rs` and `crates/**/*.rs`, minus `src/arch/**` and
the file that defines `PerCpu` (`crates/core/src/smp/per_cpu.rs`: the
declaration, its layout assertions, and its host tests), with comments
stripped, and fails on:

  - a `.current` field read, one not followed by `(` (a call) and not the
    target of a plain `=` (a write), in a file that names `PerCpu`,
    `per_cpu_init` or `per_cpu!`;
  - `offset_of!(PerCpu, current)`;
  - `per_cpu!(current)`.

    check_current.py [--root DIR]

It prints `<path>:<line>: reads PerCpu.current outside src/arch/ (use
arch::current_tcb)` per hit and exits 1, or prints `check_current: ok (<n>
files)`.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Sequence
from pathlib import Path

# Relative to the root, with `/` separators.
EXEMPT_DIR = "src/arch/"
EXEMPT_FILES = frozenset({"crates/core/src/smp/per_cpu.rs"})

NAMES_PERCPU = re.compile(r"\bPerCpu\b|\bper_cpu_init\b|\bper_cpu!")
# `.current` not continued by an identifier character, not a call, and not
# the left side of `=` (a write); `==` is a read.
FIELD_READ = re.compile(r"\.current\b(?!\s*\()(?!\s*=(?!=))")
OFFSET_OF = re.compile(r"\boffset_of!\s*\(\s*PerCpu\s*,\s*current\s*\)")
PER_CPU_MACRO = re.compile(r"\bper_cpu!\s*\(\s*current\s*\)")

_COMMENT = re.compile(r"//[^\n]*|/\*.*?\*/", re.S)


def strip_comments(text: str) -> str:
    """`text` with `//` and `/* */` comments blanked, newlines kept, so a
    line number in the result is the line number in `text`."""
    return _COMMENT.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), text)


def exempt(path: str) -> bool:
    """Whether `path` (relative, `/`-separated) is outside the check."""
    return path.startswith(EXEMPT_DIR) or path in EXEMPT_FILES


def find_current_reads(path: str, text: str) -> list[int]:
    """Line numbers of `text`, the file at `path` (relative, `/`-separated),
    that read `PerCpu.current` outside `src/arch/`."""
    if exempt(path):
        return []
    code = strip_comments(text)
    field = NAMES_PERCPU.search(code) is not None
    hits: list[int] = []
    for n, line in enumerate(code.splitlines(), 1):
        if (
            (field and FIELD_READ.search(line))
            or OFFSET_OF.search(line)
            or PER_CPU_MACRO.search(line)
        ):
            hits.append(n)
    return hits


def rust_files(root: Path) -> list[Path]:
    """The `.rs` files under `root/src` and `root/crates`, sorted."""
    files: list[Path] = []
    for base in (root / "src", root / "crates"):
        if base.is_dir():
            files.extend(p for p in base.rglob("*.rs") if p.is_file())
    return sorted(files)


def check(root: Path) -> tuple[list[str], int]:
    """The failure lines and the number of files read."""
    errs: list[str] = []
    files = rust_files(root)
    for p in files:
        rel = p.relative_to(root).as_posix()
        for n in find_current_reads(rel, p.read_text(encoding="utf-8", errors="replace")):
            errs.append(
                f"{rel}:{n}: reads PerCpu.current outside src/arch/ (use arch::current_tcb)"
            )
    return errs, len(files)


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0] if __doc__ else None)
    ap.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    args = ap.parse_args(sys.argv[1:] if argv is None else argv)
    errs, n = check(args.root)
    for e in errs:
        print(e)
    if errs:
        return 1
    print(f"check_current: ok ({n} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
