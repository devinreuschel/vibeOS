#!/usr/bin/env python3
"""docs/LINUX.md is a register with fixed tables (ROADMAP, How to read this).

A native interface or a deliberate divergence from Linux is listed in docs/LINUX.md, or has a
ROADMAP line naming the phase that replaces it, which SYSCALL.md cites where it describes the
divergence. How to read this counts a document as a gate only when a `make check` script verifies
its required parts, so this one checks:

- docs/LINUX.md has its Baseline, Deliberate differences, and Native interfaces sections;
- every table row there has as many cells as its header and no empty cell (`—` is a value);
- every Id is one backticked lowercase name, used once in the file;
- every Decided-in cell names a document section, a ROADMAP non-goal, or `here`;
- every native interface but `psinfo` lives under a vibeOS name (SYSCALL.md §8);
- in docs/SYSCALL.md, each bullet under a "differences from Linux" heading, and each row of
  the §2 errno table that names Linux, cites a kernel-review finding, a ROADMAP section, §2.1
  or §3.1, or a docs/LINUX.md row by its id.

The checks read structure (headings, tables, bullets), not phrases, so rewording cannot evade them.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LINUX_MD = ROOT / "docs" / "LINUX.md"
SYSCALL_MD = ROOT / "docs" / "SYSCALL.md"

SECTIONS = ("Baseline", "Deliberate differences", "Native interfaces")
ID_CELL = re.compile(r"`[a-z][a-z0-9-]*`")
DECIDED_IN = re.compile(
    r"DESIGN §|ROADMAP §|ROADMAP Non-goals|SYSCALL\.md §|VIBEFS\.md §|\bhere\b"
)
SEPARATOR_CELL = re.compile(r":?-+:?")
CITE = re.compile(r"\bF\d{3}\b|ROADMAP §|§2\.1\b|§3\.1\b")
BACKTICKED = re.compile(r"`([^`]+)`")
DIFFERENCES = re.compile(r"differences from linux", re.IGNORECASE)
# SYSCALL.md §8: where a native interface may live. `psinfo` predates the rule (ROADMAP §13.9).
NATIVE_NAMES = (
    "/proc/vibeos/",
    "/proc/<pid>/vibeos/",
    "/sys/kernel/vibeos/",
    "/vibeos/",
    "vibeos_",
    "/dev/vibeos/",
    "vibeos.",
)
NATIVE_EXCEPTIONS = ("psinfo",)
OUTSIDE_NAMES = "native interface outside the vibeOS names (SYSCALL.md §8)"


@dataclass
class Table:
    line: int  # 1-based line of the header row
    header: list[str]
    rows: list[tuple[int, list[str]]] = field(default_factory=list)


def split_row(raw: str) -> list[str]:
    """Cells of a `| a | b |` row. A `|` inside backticks or after a backslash is text."""
    text = raw.strip()
    if text.startswith("|"):
        text = text[1:]
    if text.endswith("|") and not text.endswith("\\|"):
        text = text[:-1]
    cells: list[str] = []
    cur: list[str] = []
    in_code = False
    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "\\" and i + 1 < len(text):
            cur.append(text[i : i + 2])
            i += 2
            continue
        if ch == "`":
            in_code = not in_code
        elif ch == "|" and not in_code:
            cells.append("".join(cur).strip())
            cur = []
            i += 1
            continue
        cur.append(ch)
        i += 1
    cells.append("".join(cur).strip())
    return cells


def is_separator(cells: list[str]) -> bool:
    return bool(cells) and all(SEPARATOR_CELL.fullmatch(c) for c in cells)


def parse_sections(text: str) -> tuple[dict[str, int], dict[str, list[Table]]]:
    """`## ` headings with their lines, and the tables under each."""
    headings: dict[str, int] = {}
    tables: dict[str, list[Table]] = {}
    current: str | None = None
    table: Table | None = None
    for n, raw in enumerate(text.splitlines(), start=1):
        if raw.startswith("## "):
            current = raw[3:].strip()
            headings.setdefault(current, n)
            tables.setdefault(current, [])
            table = None
            continue
        if not raw.lstrip().startswith("|"):
            table = None
            continue
        if current is None:
            continue
        cells = split_row(raw)
        if table is None:
            table = Table(n, cells)
            tables[current].append(table)
        elif not table.rows and is_separator(cells):
            continue
        else:
            table.rows.append((n, cells))
    return headings, tables


def check_linux(text: str, path: str = "docs/LINUX.md") -> tuple[list[str], set[str]]:
    """Errors in docs/LINUX.md, and the row ids it defines (without backticks)."""
    errors: list[str] = []
    headings, tables = parse_sections(text)
    for name in SECTIONS:
        if name not in headings:
            errors.append(f"{path}:1: missing section '## {name}'")
    ids: dict[str, int] = {}
    for name in SECTIONS:
        for table in tables.get(name, []):
            width = len(table.header)
            id_col = table.header.index("Id") if "Id" in table.header else None
            decided_col = table.header.index("Decided in") if "Decided in" in table.header else None
            iface_col = (
                table.header.index("Interface")
                if name == "Native interfaces" and "Interface" in table.header
                else None
            )
            for n, cells in table.rows:
                if len(cells) < width:
                    errors.append(f"{path}:{n}: row has {len(cells)} cells, its header {width}")
                for col, cell in enumerate(cells[:width]):
                    if not cell:
                        errors.append(f"{path}:{n}: empty cell in column '{table.header[col]}'")
                if id_col is not None and id_col < len(cells) and cells[id_col]:
                    cell = cells[id_col]
                    if not ID_CELL.fullmatch(cell):
                        errors.append(
                            f"{path}:{n}: id {cell!r} is not one backticked lowercase name "
                            "of letters, digits, and hyphens"
                        )
                    else:
                        rid = cell.strip("`")
                        if rid in ids:
                            errors.append(f"{path}:{n}: id {cell} repeats line {ids[rid]}")
                        else:
                            ids[rid] = n
                if decided_col is not None and decided_col < len(cells) and cells[decided_col]:
                    if not DECIDED_IN.search(cells[decided_col]):
                        errors.append(f"{path}:{n}: Decided in names no document section")
                if iface_col is not None and iface_col < len(cells) and cells[iface_col]:
                    has_id = id_col is not None and id_col < len(cells)
                    own = cells[id_col].strip("`") if has_id and id_col is not None else ""
                    iface = cells[iface_col]
                    if own not in NATIVE_EXCEPTIONS and not any(m in iface for m in NATIVE_NAMES):
                        errors.append(f"{path}:{n}: {OUTSIDE_NAMES}")
    return errors, set(ids)


def cites(text: str, ids: set[str]) -> bool:
    """True when `text` cites a finding, a ROADMAP section, §2.1 or §3.1, or a LINUX.md row."""
    flat = " ".join(text.split())
    if CITE.search(flat):
        return True
    return any(m in ids for m in BACKTICKED.findall(flat))


UNCITED = "cites no finding, ROADMAP section, §2.1 or §3.1, or docs/LINUX.md row"


def check_syscall(text: str, ids: set[str], path: str = "docs/SYSCALL.md") -> list[str]:
    """Each SYSCALL.md difference from Linux cites its fixing line or its LINUX.md row."""
    errors: list[str] = []
    lines = text.splitlines()
    heading = ""
    item: tuple[int, list[str]] | None = None
    in_errno_table = False

    def close_item() -> None:
        nonlocal item
        if item is not None and not cites(" ".join(item[1]), ids):
            errors.append(f"{path}:{item[0]}: difference from Linux {UNCITED}")
        item = None

    for n, raw in enumerate(lines, start=1):
        if raw.startswith("#"):
            close_item()
            heading = raw.lstrip("#").strip()
            in_errno_table = False
            continue
        if DIFFERENCES.search(heading):
            if raw.startswith("- "):
                close_item()
                item = (n, [raw[2:]])
                continue
            if item is not None and raw.startswith(" ") and raw.strip():
                item[1].append(raw)
                continue
            close_item()
        if heading.startswith("2.") and raw.startswith("|"):
            cells = split_row(raw)
            if not in_errno_table:
                in_errno_table = "Used" in cells
                continue
            if is_separator(cells) or len(cells) < 3:
                continue
            used = cells[2]
            if "Linux" in used and not cites(used, ids):
                errors.append(f"{path}:{n}: errno row {cells[0]} names Linux but {UNCITED}")
        elif not raw.startswith("|"):
            in_errno_table = False
    close_item()
    return errors


def main() -> int:
    linux_errors, ids = check_linux(LINUX_MD.read_text(encoding="utf-8"))
    errors = linux_errors + check_syscall(SYSCALL_MD.read_text(encoding="utf-8"), ids)
    if errors:
        for e in errors:
            print(e, file=sys.stderr)
        return 1
    print("check_linux_md: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
