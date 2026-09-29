#!/usr/bin/env python3
"""A known gap is a ROADMAP line, not a marker in the code (ROADMAP §10.9, standing gates).

Fails on TODO, FIXME, or XXX as a word in src/, crates/, user/, tests/, or scripts/ (ROADMAP §10.9)
unless the same line cites a section of the roadmap as `ROADMAP §N.M`. Whether that section
exists is doc_refs.py's check. The files are those `git ls-files -co --exclude-standard` lists
under the five trees, tracked or untracked and not ignored; a file that is not UTF-8 or holds a
NUL byte is skipped. This script and its test build the words from parts.
"""

from __future__ import annotations

import re
import subprocess
import sys
from collections.abc import Iterable
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TREES = ("src", "crates", "user", "tests", "scripts")
WORDS = ("TO" + "DO", "FIX" + "ME", "X" + "XX")
MARKER = re.compile(r"\b(" + "|".join(WORDS) + r")\b")
CITE = re.compile(r"ROADMAP §\d+\.\d+")


def listed_files(root: Path = ROOT) -> list[str]:
    """Tracked and untracked, non-ignored files under TREES."""
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z", "-co", "--exclude-standard", "--", *TREES],
        capture_output=True, check=True,
    ).stdout
    return sorted({p.decode("utf-8", errors="surrogateescape") for p in out.split(b"\0") if p})


def in_trees(path: str) -> bool:
    return path.split("/", 1)[0] in TREES and "/" in path


def check_text(path: str, text: str) -> list[str]:
    errors: list[str] = []
    for n, line in enumerate(text.splitlines(), start=1):
        m = MARKER.search(line)
        if m is not None and CITE.search(line) is None:
            errors.append(f"{path}:{n}: {m.group(1)} without a `ROADMAP §N.M` on its line")
    return errors


def check_todos(root: Path, paths: Iterable[str]) -> list[str]:
    errors: list[str] = []
    for path in paths:
        if not in_trees(path):
            continue
        try:
            data = (root / path).read_bytes()
        except OSError:
            continue  # listed but deleted from the working tree
        if b"\0" in data:
            continue
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            continue
        errors.extend(check_text(path, text))
    return errors


def main(argv: list[str] | None = None) -> int:
    del argv
    try:
        paths = listed_files(ROOT)
    except (OSError, subprocess.CalledProcessError) as e:
        print(f"check_todos: git ls-files failed: {e}", file=sys.stderr)
        return 1
    errors = check_todos(ROOT, paths)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"check_todos: ok ({len(paths)} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
