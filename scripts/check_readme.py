#!/usr/bin/env python3
"""README quickstarts (ROADMAP Phase 11 exit, DESIGN §8.4).

Fails when README.md has no x86_64 quickstart section or no aarch64
quickstart section, when the aarch64 section does not run
`make ARCH=aarch64 run` under HVF, or when a `make` command in either
section names a target the Makefile does not define.

A section is a `##` heading whose title contains `quickstart` and either
`x86_64` or `aarch64`. `make check` runs this script via the `check_*.py`
glob.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
README = "README.md"
MAKEFILE = "Makefile"
HEADING = re.compile(r"^##\s+(.+)$")
MAKE = re.compile(r"\bmake\b((?:\s+[A-Za-z_][A-Za-z0-9_]*=\S+)*)\s+(\S+)")
TARGET = re.compile(r"^([A-Za-z0-9_.+/-]+):(?!=)", re.M)
ASSIGN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=\S+$")


def makefile_targets(text: str) -> set[str]:
    """Recipe names in `text`, continuation lines already joined."""
    return set(TARGET.findall(text.replace("\\\n", " ")))


def sections(text: str) -> list[tuple[str, str]]:
    """`(title, body)` for each `##` section of `text`."""
    lines = text.splitlines()
    out: list[tuple[str, str]] = []
    i = 0
    while i < len(lines):
        m = HEADING.match(lines[i])
        if m is None:
            i += 1
            continue
        title = m.group(1).strip()
        i += 1
        body: list[str] = []
        while i < len(lines) and not HEADING.match(lines[i]):
            body.append(lines[i])
            i += 1
        out.append((title, "\n".join(body)))
    return out


def make_targets_in(body: str) -> list[str]:
    """Make goals named in `body`, skipping assignments and `#` comments."""
    found: list[str] = []
    for raw in body.splitlines():
        line = raw.replace("`", "")
        if "#" in line:
            line = line[: line.index("#")]
        for m in MAKE.finditer(line):
            goal = m.group(2)
            if ASSIGN.match(goal) or not TARGET.match(goal + ":"):
                continue
            found.append(goal)
    return found


def check(root: Path = ROOT) -> list[str]:
    """Failures as `path:line: message`."""
    readme = root / README
    makefile = root / MAKEFILE
    if not readme.is_file():
        return [f"{README}:0: missing"]
    text = readme.read_text(encoding="utf-8")
    mk = makefile.read_text(encoding="utf-8") if makefile.is_file() else ""
    targets = makefile_targets(mk)
    errs: list[str] = []
    x86 = aarch = None
    for title, body in sections(text):
        low = title.lower()
        if "quickstart" not in low:
            continue
        if "x86_64" in low:
            x86 = (title, body)
        if "aarch64" in low:
            aarch = (title, body)
    if x86 is None:
        errs.append(f"{README}:0: no x86_64 quickstart section")
    if aarch is None:
        errs.append(f"{README}:0: no aarch64 quickstart section")
    if aarch is not None:
        body = aarch[1]
        if not re.search(r"\bmake\b(?:\s+\S+)*=aarch64(?:\s+\S+)*\s+run\b", body):
            errs.append(f"{README}:0: aarch64 quickstart has no `make ARCH=aarch64 run`")
        if "hvf" not in body.lower():
            errs.append(f"{README}:0: aarch64 quickstart does not name HVF")
    for label, pair in (("x86_64", x86), ("aarch64", aarch)):
        if pair is None:
            continue
        for goal in make_targets_in(pair[1]):
            if goal not in targets:
                errs.append(
                    f"{README}:0: {label} quickstart runs `make {goal}`; "
                    f"Makefile has no such target"
                )
    return errs


def main(argv: list[str] | None = None) -> int:
    del argv
    errs = check()
    for e in errs:
        print(e, file=sys.stderr)
    if errs:
        print(f"check_readme: {len(errs)} failure(s)", file=sys.stderr)
        return 1
    print("check_readme: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
