#!/usr/bin/env python3
"""`vibeos-core` takes its atomics from the seam, `vibeos::atomic` (C-ATOMICS).

ROADMAP §10.8: loom models swap `core`'s atomics for loom's under
`cfg(loom)`, which works only where code names them through
`crates/core/src/atomic.rs`. This script fails on `core::sync::atomic`,
`std::sync::atomic`, `core::hint::spin_loop` or `std::hint::spin_loop` in
`crates/core/src/**` outside `atomic.rs` and outside test code.

- Test code is an inline module, or a declared file, under `#[cfg(test)]`
  or `#[cfg(all(test, ...))]` (and everything under it); `not(test)` and
  `any(test, ...)` are not test code.
- `core`'s atomics may type a field of a `#[repr(C)]` type whose layout a
  const `offset_of!` assertion on that field fixes (`PerCpu`): layout asm
  reads it, and it stays out of every loom model.

`PENDING` lists the files that fail at this tip; they are skipped until the
sweep that owns each converts it and removes its entry, and an entry that
now passes is an error.

    check_atomics.py [--failing]

`--failing` prints the files that fail, `PENDING` ignored, one per line.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Iterable
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CORE = "crates/core/src"
SEAM = "crates/core/src/atomic.rs"

# Files that use `core`'s atomics directly at this tip, skipped until their
# sweep (ROADMAP §10.1, C-ATOMICS) converts them; `--failing` regenerates it.
PENDING: tuple[str, ...] = (
    "crates/core/src/dev/dma.rs",
    "crates/core/src/dev/entropy.rs",
    "crates/core/src/dev/virtio.rs",
    "crates/core/src/kalloc.rs",
    "crates/core/src/log/mod.rs",
    "crates/core/src/mm/paging.rs",
    "crates/core/src/sched/thread.rs",
    "crates/core/src/smp/per_cpu.rs",
    "crates/core/src/sync/mod.rs",
    "crates/core/src/time/mod.rs",
)

BANNED = re.compile(r"\b(?:core|std)::(?:sync::atomic\b|hint::spin_loop\b)")
TEST_CFG = re.compile(r"^\s*#\[cfg\((?:test|all\(\s*test\s*(?:,[^\]]*)?\))\)\]\s*$")
MOD_FILE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")
MOD_INLINE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*\{")
PATH_ATTR = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\]')
REPR_C = re.compile(r"^\s*#\[repr\(C\b")
STRUCT = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+(\w+)")
FIELD = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:")


def code_of(line: str) -> str:
    """`line` without its `//` comment."""
    i = line.find("//")
    return line if i < 0 else line[:i]


def child_file(parent: Path, name: str, path_attr: str | None) -> Path:
    """The file a `mod name;` in `parent` declares."""
    if path_attr is not None:
        return parent.parent / path_attr
    base = parent.parent if parent.name in ("lib.rs", "mod.rs") else parent.parent / parent.stem
    f = base / f"{name}.rs"
    return f if f.is_file() else base / name / "mod.rs"


def test_files(root: Path) -> set[Path]:
    """Files declared under a test cfg, and every file declared below them."""
    out: set[Path] = set()
    core = root / CORE
    todo: list[tuple[Path, bool]] = [(core / "lib.rs", False)]
    seen: set[Path] = set()
    while todo:
        f, is_test = todo.pop()
        f = f.resolve()
        if f in seen or not f.is_file():
            continue
        seen.add(f)
        if is_test:
            out.add(f)
        lines = f.read_text(encoding="utf-8", errors="replace").splitlines()
        for i, line in enumerate(lines):
            m = MOD_FILE.match(code_of(line))
            if not m:
                continue
            attrs = []
            j = i - 1
            while j >= 0 and lines[j].strip().startswith("#["):
                attrs.append(lines[j])
                j -= 1
            path_attr = next((pm.group(1) for a in attrs if (pm := PATH_ATTR.match(a))), None)
            test = is_test or any(TEST_CFG.match(a) for a in attrs)
            todo.append((child_file(f, m.group(1), path_attr), test))
    return out


def test_lines(lines: list[str]) -> set[int]:
    """0-based indexes of lines inside inline `mod` blocks under a test cfg."""
    out: set[int] = set()
    i = 0
    while i < len(lines):
        if MOD_INLINE.match(code_of(lines[i])):
            j = i - 1
            test = False
            while j >= 0 and lines[j].strip().startswith("#["):
                test = test or bool(TEST_CFG.match(lines[j]))
                j -= 1
            if test:
                depth = 0
                k = i
                while k < len(lines):
                    c = code_of(lines[k])
                    depth += c.count("{") - c.count("}")
                    out.add(k)
                    if depth <= 0 and k > i or (depth == 0 and "}" in c and k == i):
                        break
                    k += 1
                i = k
        i += 1
    return out


def layout_fixed_lines(lines: list[str]) -> set[int]:
    """Field lines of `#[repr(C)]` structs whose field an `offset_of!` pins."""
    text = "\n".join(lines)
    out: set[int] = set()
    i = 0
    while i < len(lines):
        if REPR_C.match(lines[i]):
            k = i + 1
            while k < len(lines) and lines[k].strip().startswith("#["):
                k += 1
            sm = STRUCT.match(lines[k]) if k < len(lines) else None
            if sm and "{" in lines[k]:
                name = sm.group(1)
                depth = lines[k].count("{") - lines[k].count("}")
                k += 1
                while k < len(lines) and depth > 0:
                    fm = FIELD.match(code_of(lines[k]))
                    if depth == 1 and fm and re.search(
                            rf"offset_of!\(\s*{name}\s*,\s*{fm.group(1)}\s*\)", text):
                        out.add(k)
                    depth += lines[k].count("{") - lines[k].count("}")
                    k += 1
                i = k
                continue
        i += 1
    return out


def file_errors(rel: str, text: str, is_test_file: bool) -> list[str]:
    """`path:line: ...` for each banned use in one file."""
    if rel == SEAM or is_test_file:
        return []
    lines = text.splitlines()
    skip = test_lines(lines) | layout_fixed_lines(lines)
    errs = []
    for i, line in enumerate(lines):
        if i in skip:
            continue
        m = BANNED.search(code_of(line))
        if m:
            errs.append(f"{rel}:{i + 1}: {m.group(0)} outside the seam; use crate::atomic "
                        "(C-ATOMICS, ROADMAP §10.8)")
    return errs


def check_tree(root: Path = ROOT, pending: Iterable[str] = PENDING
               ) -> tuple[list[str], list[str]]:
    """(errors, failing files) for the tree at `root`."""
    tests = test_files(root)
    pending = set(pending)
    errors: list[str] = []
    failing: list[str] = []
    for f in sorted((root / CORE).rglob("*.rs")):
        rel = f.relative_to(root).as_posix()
        errs = file_errors(rel, f.read_text(encoding="utf-8", errors="replace"),
                           f.resolve() in tests)
        if errs:
            failing.append(rel)
            if rel not in pending:
                errors.extend(errs)
        elif rel in pending:
            errors.append(f"{rel}: listed in PENDING but passes; remove the entry")
    for rel in sorted(pending):
        if not (root / rel).is_file():
            errors.append(f"{rel}: listed in PENDING but missing")
    return errors, failing


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="check_atomics.py")
    ap.add_argument("--failing", action="store_true",
                    help="print the files that fail, PENDING ignored")
    args = ap.parse_args(argv if argv is not None else [])
    errors, failing = check_tree()
    if args.failing:
        for rel in failing:
            print(rel)
        return 0
    for e in errors:
        print(f"check_atomics: {e}", file=sys.stderr)
    if errors:
        return 1
    print(f"check_atomics: ok ({len(failing)} files pending)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
