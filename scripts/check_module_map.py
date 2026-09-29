#!/usr/bin/env python3
"""DESIGN §1.3's module map equals the tree (ROADMAP §10.3, A1).

Reads the first table under `## 1.3 Module map` in docs/DESIGN.md whose
header has a `Subsystem`, a `Portable…` and a `Kernel…` column. Paths in the
Portable column are relative to `crates/core/src/`, and paths in the Kernel
column to `src/`. A backticked token ending in `.rs`, `.S` or `.asm` is a
path; any other token is prose. `{a,b}` expands one group, `*` matches within
one path component, and `—` is an empty cell. Rules:

- R1: one row per subsystem, and one `crate` row.
- R2: every literal path exists, and every `*` token matches a file.
- R3: no file is listed twice.
- R4: every `.rs`, `.S` and `.asm` file under the two roots is listed, except
  a kernel `<s>/ktest.rs` for a row `<s>` (C-KTEST-LAYOUT).
- R5: outside `crate`, row `<s>`'s paths start with `<s>/`, except the kernel
  paths in `FLAT_OK`.
- R6: a kernel module `<n>_init` sits in a directory its portable `<n>`
  allows: `<e>/<n>.rs` allows `<e>`, `<e>/<n>/mod.rs` allows `<e>/<n>` and
  `<e>`, and a file in the `crate` row allows any. With no portable `<n>`,
  any directory passes.

Prints `check_module_map: ok`, or one error per line on stderr and exits 1.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DESIGN = ROOT / "docs" / "DESIGN.md"
CORE_SRC = ROOT / "crates" / "core" / "src"
KERNEL_SRC = ROOT / "src"

HEADING = "## 1.3 Module map"
CRATE = "crate"
EMPTY = "—"
EXTS = (".rs", ".S", ".asm")
# Kernel paths that may sit outside their row's directory: the ktest runner
# until ROADMAP §10.2's Q2 box moves it to `ktest/mod.rs`.
FLAT_OK: frozenset[str] = frozenset({"ktest.rs"})
KTEST_BODY = "ktest.rs"

_TOKEN = re.compile(r"`([^`]*)`")
_GROUP = re.compile(r"\{([^{}]*)\}")


@dataclass(frozen=True)
class Row:
    subsystem: str
    portable: tuple[str, ...]
    kernel: tuple[str, ...]


def section(text: str) -> str:
    """The body of `## 1.3 Module map`, up to the next `## ` heading."""
    lines = text.splitlines()
    for i, ln in enumerate(lines):
        if ln.strip() == HEADING:
            out: list[str] = []
            for rest in lines[i + 1:]:
                if rest.startswith("## ") or rest.startswith("# "):
                    break
                out.append(rest)
            return "\n".join(out)
    return ""


def _cells(line: str) -> list[str]:
    s = line.strip()
    if s.startswith("|"):
        s = s[1:]
    if s.endswith("|"):
        s = s[:-1]
    return [c.strip() for c in s.split("|")]


def _paths(cell: str) -> tuple[str, ...]:
    return tuple(t for t in _TOKEN.findall(cell) if t.endswith(EXTS))


def parse_rows(section: str) -> list[Row]:
    """The rows of the section's first table with the map's three columns."""
    lines = section.splitlines()
    i = 0
    while i < len(lines):
        if not lines[i].lstrip().startswith("|"):
            i += 1
            continue
        header = _cells(lines[i])
        cols = _columns(header)
        start = i
        while i < len(lines) and lines[i].lstrip().startswith("|"):
            i += 1
        if cols is None:
            continue
        s, p, k = cols
        rows: list[Row] = []
        for ln in lines[start + 2:i]:
            cells = _cells(ln)
            if len(cells) <= max(s, p, k):
                continue
            rows.append(Row(cells[s].strip("`").strip(), _paths(cells[p]), _paths(cells[k])))
        return rows
    return []


def _columns(header: list[str]) -> tuple[int, int, int] | None:
    s = p = k = -1
    for n, c in enumerate(header):
        if c == "Subsystem":
            s = n
        elif c.startswith("Portable"):
            p = n
        elif c.startswith("Kernel"):
            k = n
    return (s, p, k) if min(s, p, k) >= 0 else None


def expand(token: str) -> list[str]:
    """`token` with its one `{a,b}` group expanded; `*` is left for matching."""
    m = _GROUP.search(token)
    if m is None:
        return [token]
    return [token[: m.start()] + alt.strip() + token[m.end():] for alt in m.group(1).split(",")]


def tree_files(root: Path) -> set[str]:
    """Every `.rs`, `.S` and `.asm` file under `root`, relative, `/`-separated."""
    if not root.is_dir():
        return set()
    return {p.relative_to(root).as_posix() for p in root.rglob("*")
            if p.is_file() and p.name.endswith(EXTS)}


def _glob(pattern: str) -> re.Pattern[str]:
    return re.compile("[^/]*".join(re.escape(part) for part in pattern.split("*")) + r"\Z")


def _resolve(tokens: tuple[str, ...], files: set[str], side: str, sub: str,
             errors: list[str]) -> list[str]:
    """The files `tokens` name; R2 errors for a missing path or an empty `*`."""
    out: list[str] = []
    for tok in tokens:
        for path in expand(tok):
            if "*" in path:
                hits = sorted(f for f in files if _glob(path).match(f))
                if not hits:
                    errors.append(f"R2: {side} `{path}` (row {sub}) matches no file")
                out += hits
            elif path in files:
                out.append(path)
            else:
                errors.append(f"R2: {side} `{path}` (row {sub}) does not exist")
    return out


def _parent(path: str) -> str:
    return path.rsplit("/", 1)[0] if "/" in path else ""


def _init_name(path: str) -> tuple[str, str] | None:
    """(`<n>`, directory) of a kernel `<d>/<n>_init.rs` or `<d>/<n>_init/mod.rs`."""
    parts = path.split("/")
    if parts[-1] == "mod.rs" and len(parts) >= 2 and parts[-2].endswith("_init"):
        return parts[-2][: -len("_init")], "/".join(parts[:-2])
    if parts[-1].endswith("_init.rs"):
        return parts[-1][: -len("_init.rs")], "/".join(parts[:-1])
    return None


def _allowed(core_rows: list[tuple[str, str]]) -> dict[str, set[str] | None]:
    """Portable `<n>` -> the directories its kernel `<n>_init` may sit in;
    None when a `crate` row file allows any."""
    out: dict[str, set[str] | None] = {}
    for sub, path in core_rows:
        parts = path.split("/")
        if parts[-1] == "mod.rs":
            if len(parts) < 2:
                continue
            name, dirs = parts[-2], {"/".join(parts[:-1]), "/".join(parts[:-2])}
        else:
            name, dirs = parts[-1].rsplit(".", 1)[0], {_parent(path)}
        if sub == CRATE:
            out[name] = None
        elif name not in out:
            out[name] = set(dirs)
        else:
            prev = out[name]
            if prev is not None:
                prev |= dirs
    return out


def check(rows: list[Row], core: set[str], kernel: set[str]) -> list[str]:
    """Every rule's errors for `rows` against the two trees' file sets."""
    errors: list[str] = []
    names = [r.subsystem for r in rows]
    for n in sorted({n for n in names if names.count(n) > 1}):
        errors.append(f"R1: row {n} appears {names.count(n)} times")
    if CRATE not in names:
        errors.append(f"R1: no `{CRATE}` row")
    subs = set(names)
    core_rows: list[tuple[str, str]] = []
    kernel_rows: list[tuple[str, str]] = []
    for r in rows:
        core_rows += [(r.subsystem, p)
                      for p in _resolve(r.portable, core, "portable", r.subsystem, errors)]
        kernel_rows += [(r.subsystem, p)
                        for p in _resolve(r.kernel, kernel, "kernel", r.subsystem, errors)]
    for side, listed, files in (("portable", core_rows, core), ("kernel", kernel_rows, kernel)):
        seen: dict[str, str] = {}
        for sub, path in listed:
            if path in seen:
                errors.append(f"R3: {side} `{path}` listed twice (rows {seen[path]}, {sub})")
            seen.setdefault(path, sub)
            if sub != CRATE and not path.startswith(sub + "/") and not (
                    side == "kernel" and path in FLAT_OK):
                errors.append(f"R5: {side} `{path}` lies outside row {sub}'s directory")
        for path in sorted(files - set(seen)):
            if side == "kernel" and path.endswith("/" + KTEST_BODY) \
                    and _parent(path) in subs:
                continue
            errors.append(f"R4: {side} `{path}` is in no row")
    allowed = _allowed(core_rows)
    for _, path in kernel_rows:
        init = _init_name(path)
        if init is None:
            continue
        name, where = init
        if name not in allowed:
            continue
        dirs = allowed[name]
        if dirs is not None and where not in dirs:
            want = ", ".join(f"`{d or '.'}`" for d in sorted(dirs))
            errors.append(f"R6: kernel `{path}` sits apart from portable `{name}` "
                          f"(allowed: {want})")
    return errors


def main() -> int:
    rows = parse_rows(section(DESIGN.read_text(encoding="utf-8")))
    if not rows:
        print(f"check_module_map: no module map table under `{HEADING}` in "
              f"{DESIGN.relative_to(ROOT)}", file=sys.stderr)
        return 1
    errors = check(rows, tree_files(CORE_SRC), tree_files(KERNEL_SRC))
    if errors:
        print("\n".join(f"check_module_map: {e}" for e in errors), file=sys.stderr)
        return 1
    print("check_module_map: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
