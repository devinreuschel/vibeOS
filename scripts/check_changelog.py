#!/usr/bin/env python3
"""DOC4: changelog entries stay short and user-facing."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CHANGELOG = ROOT / "CHANGELOG.md"
MAX_LINES = 3
STYLE_NEEDLES = (
    "One or two lines per entry",
    "someone running vibeOS",
)


def entry_lengths(text: str) -> list[tuple[int, int]]:
    """Return (1-based start line, line count) for each changelog bullet."""
    found: list[tuple[int, int]] = []
    start: int | None = None
    count = 0
    for i, raw in enumerate(text.splitlines(), start=1):
        if raw.startswith("- "):
            if start is not None:
                found.append((start, count))
            start = i
            count = 1
            continue
        if start is not None:
            if raw.startswith("  "):
                count += 1
                continue
            found.append((start, count))
            start = None
            count = 0
    if start is not None:
        found.append((start, count))
    return found


def main() -> int:
    text = CHANGELOG.read_text(encoding="utf-8")
    errors: list[str] = []
    for needle in STYLE_NEEDLES:
        if needle not in text:
            errors.append(f"missing style rule: {needle!r}")
    if re.search(r"(?i)\bpaused\b", text):
        errors.append("changelog still says 'paused' (O1 superseded)")
    if "## [0.8.0]" not in text:
        errors.append("missing ## [0.8.0] section (B3)")
    if "[0.8.0]: https://github.com/devinreuschel/vibeOS/releases/tag/v0.8.0" not in text:
        errors.append("missing Keep a Changelog link ref for 0.8.0")
    for start, n in entry_lengths(text):
        if n > MAX_LINES:
            rel = CHANGELOG.relative_to(ROOT)
            errors.append(f"{rel}:{start}: entry is {n} lines (max {MAX_LINES})")
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("check_changelog: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
