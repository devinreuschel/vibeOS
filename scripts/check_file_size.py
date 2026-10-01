#!/usr/bin/env python3
"""No source file is over 1,500 lines unless an open box splits it (ROADMAP §10.3, Q5).

A source file is a Rust or assembly file (`.rs`, `.S`, `.asm`, `.inc`) that
git tracks or would track (`git ls-files --cached --others --exclude-standard`),
so ignored trees such as `target/` and `limine/` drop out. A file over `LIMIT`
lines fails unless `SPLIT_BY` names it with the key of the open ROADMAP box
that splits it. A key is a substring of exactly one ROADMAP line, which is a
box, as the needs file's keys are (`gatelib.match_key`). A row fails when its
key matches no box or several, when its box is ticked, when its file is gone
or no longer over `LIMIT` (delete the row), and when its path is listed twice.

Prints `check_file_size: ok (<n> files, <m> listed)`, or one error per line on
stderr and exits 1.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts.gatelib import Box, GateError, match_key, roadmap_boxes  # noqa: E402

LIMIT = 1500
# Rust and assembly sources.
SUFFIXES = (".rs", ".S", ".asm", ".inc")

# (repo-relative path, key of the open box that splits it). The slice that
# splits a file deletes its row.
SPLIT_BY: list[tuple[str, str]] = []

HINT = "split it, or name it in SPLIT_BY with the open box that splits it (ROADMAP §10.3, Q5)"


def count_lines(text: str) -> int:
    return len(text.splitlines())


def is_source(path: str) -> bool:
    return path.endswith(SUFFIXES)


def source_files(root: Path) -> list[str]:
    """Source files git tracks or would track under `root`, repo-relative."""
    out = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=root, capture_output=True, text=True, check=True).stdout
    return sorted({p for p in out.split("\0") if p and is_source(p) and (root / p).is_file()})


def check(sizes: dict[str, int], split_by: list[tuple[str, str]], boxes: list[Box],
          lines: list[str] | None = None) -> list[str]:
    """One message per failure. `sizes` maps each source file to its line count."""
    errors: list[str] = []
    listed: set[str] = set()
    for path, key in split_by:
        if path in listed:
            errors.append(f"SPLIT_BY: {path} is listed twice")
            continue
        listed.add(path)
        try:
            box = match_key(key, boxes, lines)
        except GateError as e:
            errors.append(f"SPLIT_BY: {path}: {e}")
        else:
            if box.ticked:
                errors.append(f"SPLIT_BY: {path}: its box (L{box.line}) is ticked")
        n = sizes.get(path)
        if n is None:
            errors.append(f"SPLIT_BY: {path} is gone; delete the row")
        elif n <= LIMIT:
            errors.append(f"SPLIT_BY: {path} has {n} lines, at most {LIMIT}; delete the row")
    for path, n in sorted(sizes.items()):
        if n > LIMIT and path not in listed:
            errors.append(f"{path}: {n} lines, over the {LIMIT}-line limit")
    return errors


def main(argv: list[str] | None = None) -> int:
    del argv
    roadmap = ROOT / "docs" / "ROADMAP.md"
    boxes = roadmap_boxes(roadmap)
    lines = roadmap.read_text(encoding="utf-8").splitlines()
    sizes = {p: count_lines((ROOT / p).read_text(encoding="utf-8", errors="replace"))
             for p in source_files(ROOT)}
    errors = check(sizes, SPLIT_BY, boxes, lines)
    if errors:
        for e in errors:
            print(e, file=sys.stderr)
        print(HINT, file=sys.stderr)
        return 1
    print(f"check_file_size: ok ({len(sizes)} files, {len(SPLIT_BY)} listed)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
