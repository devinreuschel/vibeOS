#!/usr/bin/env python3
"""A ticked box that asks for an owner decision has the owner's dated record (ROADMAP §10.9).

An asking box is a phase box (`gatelib.roadmap_boxes` with a phase) whose text, with code
spans stripped, holds `writes an OWNER DECISION block ` followed by `here` or by
`in <DESIGN|ROADMAP|SYSCALL|VIBEFS> §<N.M>`. `here` is the box's own section. The place is
that heading's text up to the next heading of the same or a higher level; DESIGN is
DESIGN.md and its topic files (`check_review_refs.doc_headings`). An asking box whose place
names no heading fails, open or ticked.

A record is a paragraph of a top-level `docs/*.md` file whose leading bold span holds
`(owner decision, YYYY-MM-DD, ...)` or `(owner position, YYYY-MM-DD, ...)`, whitespace
collapsed and parentheses matched by depth, since records hold links. Its date must be a
calendar date.

- A ticked asking box fails unless its place holds a record that names
  `ROADMAP §<the box's section>` (`§14.3` does not match `§14.30`).
- A `>` line holding `**OWNER DECISION NEEDED (review <ID>)**` fails when a record cites
  `<ID>` as `review <ID>` or `design review <ID>`, since the record replaces the block.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts.check_review_refs import DOCS, DocHeading, doc_headings  # noqa: E402
from scripts.gatelib import Box, parse_boxes  # noqa: E402

CODE_SPAN = re.compile(r"(`+).+?\1")
ASKS = re.compile(
    rf"writes an OWNER DECISION block (?:(here)\b|in ({'|'.join(DOCS)}) §(\d+(?:\.\d+)*))"
)
RECORD_OPEN = re.compile(r"\(owner (decision|position), ")
RECORD_DATE = re.compile(r"^(\d{4}-\d{2}-\d{2}),")
NEEDED = re.compile(r"^\s*>.*\*\*OWNER DECISION NEEDED \(review ([A-Z]\d{3})\)\*\*")
CITES = re.compile(r"\breview ([A-Z]\d{3})\b")
ROADMAP_PATH = "docs/ROADMAP.md"


@dataclass(frozen=True)
class Record:
    file: str
    line: int
    kind: str  # "decision" or "position"
    date: str
    body: str  # the text inside the parentheses, whitespace collapsed


def _paren_body(text: str, start: int) -> str | None:
    """The text after the `(` at `start - 1` up to its matching `)`."""
    depth = 1
    for i in range(start, len(text)):
        if text[i] == "(":
            depth += 1
        elif text[i] == ")":
            depth -= 1
            if depth == 0:
                return text[start:i]
    return None


def parse_records(path: str, text: str, errors: list[str]) -> list[Record]:
    """The records in `text`; a malformed one is appended to `errors`."""
    out: list[Record] = []
    lines = text.splitlines()
    n = 0
    while n < len(lines):
        if not lines[n].strip():
            n += 1
            continue
        start = n
        while n < len(lines) and lines[n].strip():
            n += 1
        para = " ".join(" ".join(lines[start:n]).split())
        if not para.startswith("**"):
            continue
        end = para.find("**", 2)
        lead = para[2:end] if end >= 0 else para[2:]
        for m in RECORD_OPEN.finditer(lead):
            body = _paren_body(lead, m.start() + 1)
            rest = lead[m.end():] if body is None else body[m.end() - m.start() - 1:]
            d = RECORD_DATE.match(rest)
            try:
                if d is None:
                    raise ValueError("no YYYY-MM-DD date")
                date.fromisoformat(d.group(1))
            except ValueError as e:
                errors.append(f"{path}:{start + 1}: owner {m.group(1)} record: bad date ({e})")
                continue
            out.append(Record(path, start + 1, m.group(1), d.group(1), body or rest))
    return out


def place_range(headings: list[DocHeading], number: str) -> tuple[str, int, int] | None:
    """(file, first line, last line) of the heading numbered `number`, up to the next
    heading of the same or a higher level in its file."""
    for i, h in enumerate(headings):
        if h.number != number:
            continue
        end = sys.maxsize
        for later in headings[i + 1:]:
            if later.file != h.file:
                break
            if later.level <= h.level:
                end = later.line - 1
                break
        return h.file, h.line, end
    return None


def names_section(body: str, section: str) -> bool:
    return re.search(rf"ROADMAP §{re.escape(section)}(?!\.?\d)", body) is not None


def check_owner_decisions(boxes: list[Box], files: dict[str, str]) -> list[str]:
    """`files` maps each top-level docs/*.md path, and every DOCS path, to its text."""
    errors: list[str] = []
    records: list[Record] = []
    for path in sorted(files):
        records.extend(parse_records(path, files[path], errors))
    index = {doc: doc_headings(doc, files) for doc in DOCS}
    for b in boxes:
        if b.phase is None:
            continue
        m = ASKS.search(CODE_SPAN.sub("", b.text))
        if m is None:
            continue
        words = " ".join(b.text.split()[:8])
        if m.group(1):
            doc, number = "ROADMAP", b.section
        else:
            doc, number = m.group(2), m.group(3)
        where = f"{doc} §{number}" if number else f"{doc} (no section)"
        place = place_range(index[doc], number) if number else None
        if place is None:
            errors.append(f"ROADMAP.md:{b.line}: OWNER DECISION place {where} names no "
                          f"heading: {words}")
            continue
        if not b.ticked:
            continue
        file, first, last = place
        held = [r for r in records if r.file == file and first <= r.line <= last]
        if not any(names_section(r.body, b.section or "") for r in held):
            errors.append(f"ROADMAP.md:{b.line}: ticked, and {where} holds no dated owner "
                          f"record naming ROADMAP §{b.section}: {words}")
    cited = {rid for r in records for rid in CITES.findall(r.body)}
    for path in sorted(files):
        for n, raw in enumerate(files[path].splitlines(), start=1):
            nd = NEEDED.match(raw)
            if nd is not None and nd.group(1) in cited:
                errors.append(f"{path}:{n}: OWNER DECISION NEEDED (review {nd.group(1)}) is "
                              f"answered by a dated owner record; the record replaces it")
    return errors


def tree_files() -> dict[str, str]:
    paths = {str(p.relative_to(ROOT)) for p in (ROOT / "docs").glob("*.md")}
    paths |= {p for ps in DOCS.values() for p in ps}
    return {p: (ROOT / p).read_text(encoding="utf-8") for p in sorted(paths)
            if (ROOT / p).is_file()}


def main(argv: list[str] | None = None) -> int:
    del argv
    files = tree_files()
    errors = check_owner_decisions(parse_boxes(files[ROADMAP_PATH]), files)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("check_owner_decisions: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
