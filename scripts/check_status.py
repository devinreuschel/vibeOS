#!/usr/bin/env python3
"""Box status notes agree with their checkboxes (ROADMAP §10.9, How to read this).

Over the phase boxes (`gatelib.roadmap_boxes` with a phase; the lines after
`# Beyond` are proposals), with code spans stripped first:

- a ticked box fails when it holds a deferral or reopen note: `lands in §`,
  `deferred to §`, or `Reopened by`. Prose such as "lands in the allocator"
  (no `§`) is not a note;
- an open box fails when a `lands in` note names sections of two phases. The
  note's references are `§...` items joined by `, `, ` and `, ` or `, and
  `, and `; each one's phase is its number before the first dot, so
  "§10.2 and §10.11" passes and "§10.2 and §11.1" fails.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts.gatelib import ROADMAP, Box, roadmap_boxes  # noqa: E402

CODE_SPAN = re.compile(r"(`+).+?\1")
TICKED_NOTE = re.compile(r"lands in §|deferred to §|Reopened by")
REF = r"§\d+(?:\.\d+)*"
LANDS_NOTE = re.compile(rf"\blands in ({REF}(?:(?:,? and |,? or |, ){REF})*)")
REF_NUM = re.compile(r"§(\d+)")


def strip_code(text: str) -> str:
    return CODE_SPAN.sub("", text)


def note_phases(text: str) -> list[set[int]]:
    """The phases of each `lands in` note in `text` (code spans already stripped)."""
    return [{int(n) for n in REF_NUM.findall(m.group(1))} for m in LANDS_NOTE.finditer(text)]


def check_status(boxes: list[Box]) -> list[str]:
    errors: list[str] = []
    for b in boxes:
        if b.phase is None:
            continue
        text = strip_code(b.text)
        words = " ".join(b.text.split()[:8])
        if b.ticked:
            m = TICKED_NOTE.search(text)
            if m is not None:
                errors.append(f"ROADMAP.md:{b.line}: ticked box holds {m.group(0)!r}: {words}")
            continue
        for phases in note_phases(text):
            if len(phases) > 1:
                named = ", ".join(str(p) for p in sorted(phases))
                errors.append(f"ROADMAP.md:{b.line}: `lands in` note names Phases {named}: "
                              f"{words}")
    return errors


def main(argv: list[str] | None = None) -> int:
    del argv
    errors = check_status(roadmap_boxes(ROADMAP))
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("check_status: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
