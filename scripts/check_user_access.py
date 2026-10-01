#!/usr/bin/env python3
"""User memory is written only through the fill API and the accessors (ROADMAP §10.6).

The fill-API box: a space no thread runs is written only through the fill
API, the module pair `FILL_MODULE`, whose physmap primitives (`PRIMITIVES`)
are private to it, and `stac`, which lifts SMAP, runs only in the user
accessors' module, `ACCESSOR_MODULE` (DESIGN §5.1, invariant I7). This
script reads every `.rs` file under `src/` and `crates/`, and every `.S`
file for rule B, and fails on:

- A: a listed primitive, as a whole word in Rust code with comments and
  string literals stripped, in a file outside `FILL_MODULE`:
  `<path>:<line>: <name> outside the fill API (ROADMAP §10.6)`;
- B: `stac`, as a whole word in code or in a string literal (an `asm!`
  template), comments stripped, in a file other than `ACCESSOR_MODULE`:
  `<path>:<line>: stac outside the accessor module (ROADMAP §10.6)`;
- C: the list and the module disagree: a listed primitive with no `fn`
  in the portable fill module, a `physmap_*` fn there that the list does
  not name, or a missing module file.

    check_user_access.py [--root DIR]

Prints `check_user_access: ok`, or one error per line and exits 1.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Sequence
from pathlib import Path

# The physmap user-memory primitives, private fns of the portable fill
# module.
PRIMITIVES: tuple[str, ...] = ("physmap_write", "physmap_zero", "physmap_copy")
# The fill API: its portable half (which defines the primitives) and its
# kernel half. Relative to the root, `/`-separated.
FILL_MODULE: tuple[str, str] = ("crates/core/src/proc/fill.rs", "src/proc/fill_init.rs")
# The user accessors' module, the one place `stac` may appear.
ACCESSOR_MODULE = "src/arch/x86_64/uaccess.rs"

SCAN_DIRS = ("src", "crates")

_STAC = re.compile(r"\bstac\b")
_RAW = re.compile(r'b?r(#*)"')
_CHAR = re.compile(r"'(\\.[^']*|[^'\\\n])'")
_PHYSMAP_FN = re.compile(r"\bfn\s+(physmap_\w+)\b")


def _blank(s: str) -> str:
    """`s` with every character but newlines turned into a space."""
    return re.sub(r"[^\n]", " ", s)


def strip_rust(text: str) -> tuple[str, str]:
    """Two copies of the Rust source `text`, line numbers kept: one with
    comments blanked, and one with comments and string literals blanked.
    Handles nested block comments, raw strings, byte strings, and the
    difference between a char literal and a lifetime."""
    code: list[str] = []
    bare: list[str] = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            code.append(_blank(text[i:j]))
            bare.append(_blank(text[i:j]))
            i = j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            code.append(_blank(text[i:j]))
            bare.append(_blank(text[i:j]))
            i = j
        elif c in "br" and (m := _RAW.match(text, i)) and not _ident_before(text, i):
            close = '"' + m.group(1)
            j = text.find(close, m.end())
            j = n if j < 0 else j + len(close)
            code.append(text[i:j])
            bare.append(_blank(text[i:j]))
            i = j
        elif c == '"' or (c == "b" and text.startswith('b"', i) and not _ident_before(text, i)):
            j = i + (2 if c == "b" else 1)
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            j = min(j + 1, n)
            code.append(text[i:j])
            bare.append(_blank(text[i:j]))
            i = j
        elif c == "'":
            m = _CHAR.match(text, i)
            if m:
                code.append(text[i:m.end()])
                bare.append(_blank(text[i:m.end()]))
                i = m.end()
            else:
                code.append(c)
                bare.append(c)
                i += 1
        else:
            code.append(c)
            bare.append(c)
            i += 1
    return "".join(code), "".join(bare)


def _ident_before(text: str, i: int) -> bool:
    """Whether the character before `i` continues an identifier, so `r"`
    or `b"` there is not a literal's prefix."""
    return i > 0 and (text[i - 1].isalnum() or text[i - 1] == "_")


def strip_asm(text: str) -> str:
    """The `.S` source `text` with `//`, `/* */` and `#` comments blanked,
    line numbers kept. A `#` that starts a preprocessor line is a comment
    here too: neither holds an instruction."""
    text = re.sub(r"/\*.*?\*/", lambda m: _blank(m.group(0)), text, flags=re.S)
    return re.sub(r"(//|#)[^\n]*", lambda m: _blank(m.group(0)), text)


def line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


def check_file(rel: str, text: str) -> list[str]:
    """Rule A and B failures in the file at `rel` (relative, `/`-separated)."""
    errs: list[str] = []
    if rel.endswith(".S"):
        if rel != ACCESSOR_MODULE:
            code = strip_asm(text)
            for m in _STAC.finditer(code):
                errs.append(f"{rel}:{line_of(code, m.start())}: stac outside the accessor module "
                            "(ROADMAP §10.6)")
        return errs
    code, bare = strip_rust(text)
    if rel not in FILL_MODULE:
        for name in PRIMITIVES:
            for m in re.finditer(rf"\b{re.escape(name)}\b", bare):
                errs.append(f"{rel}:{line_of(bare, m.start())}: {name} outside the fill API "
                            "(ROADMAP §10.6)")
    if rel != ACCESSOR_MODULE:
        for m in _STAC.finditer(code):
            errs.append(f"{rel}:{line_of(code, m.start())}: stac outside the accessor module "
                        "(ROADMAP §10.6)")
    return errs


def check_module(root: Path) -> list[str]:
    """Rule C: the list against the fill module's definitions."""
    errs: list[str] = []
    for rel in FILL_MODULE:
        if not (root / rel).is_file():
            errs.append(f"{rel}: the fill API module is missing (ROADMAP §10.6)")
    portable = root / FILL_MODULE[0]
    if not portable.is_file():
        return errs
    _, bare = strip_rust(portable.read_text(encoding="utf-8", errors="replace"))
    defined = set(_PHYSMAP_FN.findall(bare))
    for name in PRIMITIVES:
        if name not in defined:
            errs.append(f"{FILL_MODULE[0]}: listed primitive {name} has no fn here "
                        "(PRIMITIVES, ROADMAP §10.6)")
    for name in sorted(defined - set(PRIMITIVES)):
        errs.append(f"{FILL_MODULE[0]}: fn {name} is not in PRIMITIVES (ROADMAP §10.6)")
    return errs


def source_files(root: Path) -> list[Path]:
    """The `.rs` and `.S` files under `root/src` and `root/crates`, sorted."""
    files: list[Path] = []
    for d in SCAN_DIRS:
        base = root / d
        if base.is_dir():
            files.extend(p for p in base.rglob("*") if p.is_file() and p.suffix in (".rs", ".S"))
    return sorted(files)


def check(root: Path) -> list[str]:
    """Every failure under `root`, rule C first."""
    errs = check_module(root)
    for p in source_files(root):
        rel = p.relative_to(root).as_posix()
        errs.extend(check_file(rel, p.read_text(encoding="utf-8", errors="replace")))
    return errs


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0] if __doc__ else None)
    ap.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    args = ap.parse_args(sys.argv[1:] if argv is None else argv)
    errs = check(args.root)
    for e in errs:
        print(e, file=sys.stderr)
    if errs:
        return 1
    print("check_user_access: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
