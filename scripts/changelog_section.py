#!/usr/bin/env python3
"""Print one Keep a Changelog section from CHANGELOG.md.

usage: changelog_section.py <version>
       changelog_section.py --tag v0.8.0
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CHANGELOG = ROOT / "CHANGELOG.md"
LINK_REF = re.compile(r"^\[[^\]]+\]:\s+\S")


def version_from_tag(tag: str) -> str:
    name = tag.strip()
    if name.startswith("refs/tags/"):
        name = name[len("refs/tags/") :]
    if name.startswith("v"):
        name = name[1:]
    if not name:
        raise ValueError(f"empty version from tag {tag!r}")
    return name


def extract_section(text: str, version: str) -> str:
    heading = f"## [{version}]"
    lines = text.splitlines(keepends=True)
    start: int | None = None
    for i, line in enumerate(lines):
        if line.startswith(heading):
            start = i
            break
    if start is None:
        raise LookupError(f"no changelog section {heading}")
    end = len(lines)
    for j in range(start + 1, len(lines)):
        if lines[j].startswith("## ["):
            end = j
            break
        if LINK_REF.match(lines[j]):
            end = j
            break
    body = "".join(lines[start:end]).strip() + "\n"
    if not body.startswith(heading):
        raise LookupError(f"truncated changelog section {heading}")
    return body


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", nargs="?", help="section version, e.g. 0.8.0")
    parser.add_argument("--tag", help="git tag (v0.8.0) or ref (refs/tags/v0.8.0)")
    parser.add_argument(
        "--file",
        type=Path,
        default=CHANGELOG,
        help="changelog path (default: repo CHANGELOG.md)",
    )
    args = parser.parse_args(argv)
    if args.version and args.tag:
        parser.error("pass exactly one of VERSION or --tag")
    if args.tag:
        version = version_from_tag(args.tag)
    elif args.version:
        version = args.version
    else:
        parser.error("pass exactly one of VERSION or --tag")
    try:
        sys.stdout.write(extract_section(args.file.read_text(encoding="utf-8"), version))
    except (LookupError, ValueError, OSError) as exc:
        print(exc, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
