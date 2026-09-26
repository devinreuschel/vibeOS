#!/usr/bin/env python3
"""Removed identifiers and paths stay removed (ROADMAP §10.9, C-GONE).

`GONE` lists what boxes removed, each row naming its box by a key of the
needs-file form (a substring of exactly one box line of docs/ROADMAP.md). The
check fails when a listed name appears in `src/`, `crates/`, `user/`,
`tests/`, `scripts/`, `.github/`, the `Makefile`, `build.rs`, or `setup.sh`,
outside this file and `tests/harness/test_gone.py`. `docs/` and
`CHANGELOG.md` are dated prose and are not read.

A row is one of:
- an identifier, which fails as a whole word;
- a path or glob (it holds `/` or a glob character), which fails when a file
  matches it, and a plain path also when its text appears in a file;
- a definition, `<path>: <kind> <name>` (`src/fs/fat_init.rs: fn route`),
  which fails only when that file still defines `<name>` as `<kind>`, so the
  bare word stays legal elsewhere.
A file whose basename equals a row fails too.

A deleting commit adds its rows here and names this script in its `Proves:`
line (ROADMAP, How to read this).
"""

from __future__ import annotations

import fnmatch
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402

# (identifier, path, glob, or definition row; box key). One row per line,
# sorted. A key stops before any gone name, since tests/gates/ and this
# file's own keys would otherwise have to name it.
GONE: list[tuple[str, str]] = [
    ("EXIT_STATUS", "one ring-3 entry model:"),
    ("IN_USER", "one ring-3 entry model:"),
    ("STDOUT_LEN", "one ring-3 entry model:"),
    ("USER_JMP", "one ring-3 entry model:"),
    ("bind_current", "one ring-3 entry model:"),
    ("bind_probe", "one ring-3 entry model:"),
    ("boot_hello", "one ring-3 entry model:"),
    ("capture_stdout", "one ring-3 entry model:"),
    ("flags_if_on", "one ring-3 entry model:"),
    ("longjmp_user", "one ring-3 entry model:"),
    ("reset_stdout", "one ring-3 entry model:"),
    ("return_status_or_die", "one ring-3 entry model:"),
    ("run_path", "one ring-3 entry model:"),
    ("run_user", "one ring-3 entry model:"),
    ("set_exit_status", "one ring-3 entry model:"),
    ("src/syscall_init.rs: static STDOUT", "one ring-3 entry model:"),
    ("stdout_bytes", "one ring-3 entry model:"),
    ("unbind_current", "one ring-3 entry model:"),
    ("unbind_probe", "one ring-3 entry model:"),
    ("vibeos_user_longjmp", "one ring-3 entry model:"),
    ("vibeos_user_setjmp", "one ring-3 entry model:"),
    ("with_user_as", "one ring-3 entry model:"),
    ("IrqsOffOnDrop", "the in-guest registry runs in production's interrupt context"),
    ("SMP2_TCG_PER_CPU_READY_HEAD_FLAKE",
     "the `-smp 2` `per_cpu_bsp: ready_head should be empty` assertion deleted"),
    ("SMP2_TCG_PER_CPU_READY_HEAD_FLAKE_ERROR",
     "the `-smp 2` `per_cpu_bsp: ready_head should be empty` assertion deleted"),
    ("with_timer", "the in-guest registry runs in production's interrupt context"),
]

SCOPE = ["src", "crates", "user", "tests", "scripts", ".github", "Makefile", "build.rs",
         "setup.sh"]
EXEMPT = frozenset({"scripts/check_gone.py", "tests/harness/test_gone.py"})
DEFINITION = re.compile(r"^(\S+): (fn|struct|enum|union|trait|type|const|static|mod|macro"
                        r"|def|class) ([A-Za-z_][A-Za-z0-9_]*)$")
GLOB_CHARS = frozenset("*?[")


def _word(name: str) -> re.Pattern[str]:
    return re.compile(r"(?<![A-Za-z0-9_])" + re.escape(name) + r"(?![A-Za-z0-9_])")


def _definition(kind: str, name: str) -> re.Pattern[str]:
    if kind == "macro":
        return re.compile(r"macro_rules!\s*" + re.escape(name) + r"(?![A-Za-z0-9_])")
    return re.compile(r"(?<![A-Za-z0-9_])" + kind + r"\s+" + re.escape(name)
                      + r"(?![A-Za-z0-9_])")


def _hits(pattern: re.Pattern[str], files: dict[str, str]) -> list[tuple[str, int]]:
    out: list[tuple[str, int]] = []
    for path in sorted(files):
        for n, line in enumerate(files[path].splitlines(), start=1):
            if pattern.search(line):
                out.append((path, n))
    return out


def find(gone: list[tuple[str, str]], files: dict[str, str], paths: list[str]) -> list[str]:
    """Each place a row of `gone` still appears. `files` maps a scoped path to
    its text; `paths` lists every scoped path, unreadable ones included."""
    errors: list[str] = []
    for row, key in gone:
        why = f"{row!r} is gone ({key})"
        d = DEFINITION.match(row)
        if d is not None:
            path, kind, name = d.groups()
            text = files.get(path)
            if text is not None:
                for n, line in enumerate(text.splitlines(), start=1):
                    if _definition(kind, name).search(line):
                        errors.append(f"{path}:{n}: defines {kind} {name}: {why}")
            continue
        for p in paths:
            if p.rsplit("/", 1)[-1] == row:
                errors.append(f"{p}: file named {why}")
        if "/" in row or GLOB_CHARS & set(row):
            for p in paths:
                if fnmatch.fnmatchcase(p, row) or fnmatch.fnmatchcase(p, row.rstrip("/") + "/*"):
                    errors.append(f"{p}: path {why}")
            if GLOB_CHARS & set(row):
                continue
            pattern = re.compile(re.escape(row))
        else:
            pattern = _word(row)
        for path, n in _hits(pattern, files):
            errors.append(f"{path}:{n}: {why}")
    return errors


def check_table(gone: list[tuple[str, str]], roadmap_text: str) -> list[str]:
    """Each row names its box by a key of one box line, and no row repeats."""
    errors: list[str] = []
    boxes = gatelib.parse_boxes(roadmap_text)
    lines = roadmap_text.splitlines()
    seen: set[str] = set()
    for row, key in gone:
        if row in seen:
            errors.append(f"GONE: {row!r} listed twice")
        seen.add(row)
        try:
            gatelib.match_key(key, boxes, lines)
        except gatelib.GateError as e:
            errors.append(f"GONE: {row!r}: {e}")
    return errors


def scoped_files(repo: Path = ROOT) -> tuple[dict[str, str], list[str]]:
    """Tracked and untracked, not ignored, files in scope, less the exempt."""
    out = gatelib.git(repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard",
                      "--", *SCOPE)
    paths = sorted({p for p in out.split("\0") if p and p not in EXEMPT})
    files: dict[str, str] = {}
    for p in paths:
        try:
            files[p] = (repo / p).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
    return files, paths


def main(argv: list[str] | None = None) -> int:
    if argv:
        print("usage: check_gone.py", file=sys.stderr)
        return 2
    errors = check_table(GONE, gatelib.ROADMAP.read_text(encoding="utf-8"))
    files, paths = scoped_files()
    errors += find(GONE, files, paths)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"check_gone: ok ({len(GONE)} rows, {len(paths)} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
