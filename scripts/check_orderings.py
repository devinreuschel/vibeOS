#!/usr/bin/env python3
"""Every atomic ordering but `SeqCst` says what it pairs with (ROADMAP §11.7).

A `Relaxed`, `Acquire`, `Release` or `AcqRel` ordering outside test code,
fences included, carries a one-line comment naming the access it pairs with
or saying it pairs with none, so a weakly ordered port starts from written
pairs. This script reads the `.rs` files under `src/`, `crates/` and `user/`,
with comments and literals blanked, and fails on:

  - an ordering, `Ordering::Acquire` or the same name after any path ending
    in `Ordering` (`AtomicOrdering::Acquire`), whose statement has no `//`
    line that names the ordering and says `pairs with` and what: an access
    (`pairs with the Release store in unlock`) or nothing (`pairs with
    nothing`). The statement starts after the last `;`, `{` or `}` before
    the ordering. After a `{` it starts on the brace's line, so a comment
    above a `match`, a struct literal or a block covers what starts inside
    it, unless the head before the brace holds an ordering of its own
    (`if X.load(..) {`), which that comment is about. A `}` followed by
    `.`, `?`, `,`, `)`, `]` or `else` closes a block inside the statement
    (`unsafe { .. }.load(..)`, `} else if`), so the statement starts before
    that block. Its comments are the run of `//` lines just above its first
    line and the comment on each of its lines up to the ordering's. One
    line may serve two orderings of one statement, such as a
    compare-exchange's success and failure, by naming both.
  - a `use` that imports an ordering by its bare name (`Ordering::*`,
    `Ordering::{Acquire, ..}`) or renames atomic `Ordering` to a name not
    ending in `Ordering`, either of which would hide an ordering from the
    first rule.

Test code is not read: a `ktest.rs` file or a file under a `ktest/`
directory (`check_test_hooks.is_test_source`), a file vibeos-core declares
under `#[cfg(test)]` or `#[cfg(all(test, ..))]` (`check_atomics.test_files`),
and any item under one of those attributes. A test hook behind a test-only
feature is read: it pairs with the kernel's accesses.

    check_orderings.py [--root DIR]

It prints `<path>:<line>: ...` per failure and exits 1, or prints
`check_orderings: ok (<n> orderings)`.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Sequence
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import check_atomics, check_test_hooks  # noqa: E402
from scripts.check_cells import line_of, strip_comments  # noqa: E402

SCOPE = ("src", "crates", "user")
NAMES = "Relaxed|Acquire|Release|AcqRel"

SITE = re.compile(rf"\b\w*Ordering::({NAMES})\b")
PAIRS = re.compile(r"\bpairs with\s+\S")
USE = re.compile(r"\buse\b[^;]*;")
BARE = re.compile(rf"\bOrdering::(?:\*|\{{[^}}]*\b(?:{NAMES})\b|(?:{NAMES})\b)")
RENAME = re.compile(r"\batomic::(?:\{[^}]*?)?\bOrdering\s+as\s+(\w+)")
# What may follow a `}` that closes a block inside a statement.
GOES_ON = re.compile(r"\s*(?:[.?,)\]]|else\b)")


def test_items(lines: list[str], codes: list[str]) -> set[int]:
    """0-based indexes of the lines of each item under a test cfg: from the
    attribute to the `;` or `,` that ends the item, or the `}` that closes
    its body. `codes` is `lines` with comments and literals blanked."""
    out: set[int] = set()
    i = 0
    while i < len(lines):
        if not check_atomics.TEST_CFG.match(lines[i]):
            i += 1
            continue
        depth, end = 0, len(lines) - 1
        for k in range(i + 1, len(lines)):
            for c in codes[k]:
                depth += (c in "([{") - (c in ")]}")
                if depth < 0 or (depth == 0 and c in ";,}"):
                    break
            else:
                continue
            end = k
            break
        out.update(range(i, end + 1))
        i = end + 1
    return out


def block_open(code: str, close: int) -> int:
    """Index of the `{` that the `}` at `close` closes, or 0."""
    depth = 0
    for j in range(close, -1, -1):
        depth += (code[j] == "}") - (code[j] == "{")
        if depth == 0:
            return j
    return 0


def anchor_of(code: str, pos: int) -> int:
    """Index of the `;`, `{` or `}` the statement holding `pos` starts
    after, or -1."""
    end = pos
    while True:
        anchor = max(code.rfind(c, 0, end) for c in ";{}")
        if anchor < 0 or code[anchor] != "}" or not GOES_ON.match(code, anchor + 1):
            return anchor
        end = block_open(code, anchor)


def first_line(code: str, pos: int) -> int:
    """The 0-based first line of the statement holding `pos`."""
    anchor = anchor_of(code, pos)
    if anchor >= 0 and code[anchor] == "{":
        head = anchor_of(code, anchor) + 1
        if not SITE.search(code, head, anchor):
            return line_of(code, anchor) - 1
    i = anchor + 1
    while i < pos and code[i].isspace():
        i += 1
    return line_of(code, i) - 1


def comment_of(line: str, code: str) -> str:
    """The `//` comment that ends `line`, or "". `code` is `line` with
    comments and literals blanked, so a `//` inside a string is no comment."""
    i = line.find("//")
    while i >= 0:
        if not code[i:].strip():
            return line[i:]
        i = line.find("//", i + 1)
    return ""


def statement_comments(lines: list[str], codes: list[str], first: int, last: int) -> list[str]:
    """The comments of a statement whose lines run from `first` to `last`
    (0-based): the `//` lines just above `first`, then each line's comment."""
    out: list[str] = []
    j = first - 1
    while j >= 0 and lines[j].lstrip().startswith("//") and not codes[j].strip():
        out.append(lines[j])
        j -= 1
    out.extend(comment_of(lines[k], codes[k]) for k in range(first, last + 1))
    return out


def names_pair(comments: list[str], name: str) -> bool:
    """Whether one comment line names `name` and what it pairs with."""
    word = re.compile(rf"\b{name}\b")
    return any(word.search(c) and PAIRS.search(c) for c in comments)


def file_errors(rel: str, text: str) -> tuple[list[str], int]:
    """The failures in one file outside its test items, and the number of
    orderings read."""
    code = strip_comments(text)
    lines, codes = text.split("\n"), code.split("\n")
    skip = test_items(lines, codes)
    errs: list[str] = []
    n = 0
    for m in SITE.finditer(code):
        last = line_of(code, m.start()) - 1
        if last in skip:
            continue
        n += 1
        comments = statement_comments(lines, codes, first_line(code, m.start()), last)
        if not names_pair(comments, m.group(1)):
            errs.append(f"{rel}:{last + 1}: {m.group(1)} without a comment naming what it "
                        f"pairs with, or `{m.group(1)}: pairs with nothing` (ROADMAP §11.7)")
    for m in USE.finditer(code):
        line = line_of(code, m.start())
        if line - 1 in skip:
            continue
        alias = RENAME.search(m.group(0))
        if BARE.search(m.group(0)):
            errs.append(f"{rel}:{line}: imports an ordering by its bare name; write "
                        "`Ordering::<name>` (ROADMAP §11.7)")
        elif alias and not alias.group(1).endswith("Ordering"):
            errs.append(f"{rel}:{line}: renames `Ordering` to `{alias.group(1)}`; keep a name "
                        "ending in `Ordering` (ROADMAP §11.7)")
    return errs, n


def rust_files(root: Path) -> list[Path]:
    """The `.rs` files under each SCOPE directory of `root`, sorted, no `target/`."""
    files: list[Path] = []
    for d in SCOPE:
        if (root / d).is_dir():
            files.extend(p for p in (root / d).rglob("*.rs")
                         if p.is_file() and "target" not in p.relative_to(root).parts)
    return sorted(files)


def check(root: Path) -> tuple[list[str], int]:
    """The failures under `root` and the number of orderings read."""
    tests = check_atomics.test_files(root)
    errs: list[str] = []
    n = 0
    for p in rust_files(root):
        rel = p.relative_to(root).as_posix()
        if check_test_hooks.is_test_source(rel) or p.resolve() in tests:
            continue
        e, k = file_errors(rel, p.read_text(encoding="utf-8", errors="replace"))
        errs += e
        n += k
    return errs, n


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="check_orderings.py")
    ap.add_argument("--root", type=Path, default=ROOT)
    args = ap.parse_args(sys.argv[1:] if argv is None else argv)
    errs, n = check(args.root)
    for e in errs:
        print(e)
    if errs:
        return 1
    print(f"check_orderings: ok ({n} orderings)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
