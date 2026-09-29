#!/usr/bin/env python3
"""The review-issue index against the roadmap (ROADMAP Phase 10 exit gate,
the line on [reviews/issues/README.md](reviews/issues/README.md)).

The Status column of `docs/reviews/issues/README.md` is the only status of a
code. This script, which `make check` runs, fails on:

- a Status cell other than `proposed`, `in progress (#N[, #N]…)`,
  `implemented (#N[, #N]…)`, `declined: <reason>`, or `superseded: <what>`;
- `implemented`, `declined`, or `superseded` while an open box before
  `## Phase 11:` cites the code, and `proposed` or `in progress` while none
  does;
- a `proposed` or `in progress` plan with no `**ROADMAP:**` line above its
  first table, or one that names no `§` section or lacks `Superseded here:`;
- a row whose plan file is missing;
- an open citing box whose `lands in §M.x` note has M > 10: a box that cites
  a code is not deferred past Phase 10.

A box cites a code when the code starts an item of a top-level
comma-separated list inside a parenthesized group on the box's line, after
code spans are stripped: `(Q1, F147)` cites Q1 and F147, `(P1, R1)` cites
both, and `(A2 landed the crate; `cfg(target_arch)` stays)` cites A2, while
`` `(Q1, F147)` `` in a code span cites nothing.

`--closed`, which the exit gate's §10.9 gate-map entry runs, also fails on
any `proposed` or `in progress` row.

Standard library only.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402
from scripts.check_gates import lands_in_sections, strip_code_spans  # noqa: E402

ISSUES = ROOT / "docs" / "reviews" / "issues"
INDEX = ISSUES / "README.md"
ROW = re.compile(r"^\|\s*\[([A-Z]+[0-9]+)\]\(([^)]+)\)\s*\|")
PRS = r"\(#\d+(?:, #\d+)*\)"
STATUS = re.compile(
    rf"^(?:proposed|in progress {PRS}|implemented {PRS}|declined: \S.*|superseded: \S.*)$"
)
OPEN_STATES = ("proposed", "in progress")
LAST_PHASE = 10


@dataclass(frozen=True)
class Row:
    code: str
    plan: str  # the plan's file name, relative to the index
    status: str  # the cell without its backticks
    line: int

    @property
    def state(self) -> str:
        """`proposed`, `in progress`, `implemented`, `declined`, `superseded`."""
        for s in ("proposed", "in progress", "implemented", "declined", "superseded"):
            if self.status.startswith(s):
                return s
        return ""


def parse_index(text: str) -> list[Row]:
    """The table rows of the index: code, plan link, and last cell."""
    rows: list[Row] = []
    for n, raw in enumerate(text.splitlines(), start=1):
        m = ROW.match(raw)
        if m is None:
            continue
        cells = [c.strip() for c in raw.strip().strip("|").split("|")]
        status = cells[-1].strip()
        if len(status) >= 2 and status[0] == status[-1] == "`":
            status = status[1:-1].strip()
        rows.append(Row(m.group(1), m.group(2), status, n))
    return rows


def groups(text: str) -> list[str]:
    """The contents of every parenthesized group, nested ones included."""
    out: list[str] = []
    stack: list[int] = []
    for i, ch in enumerate(text):
        if ch == "(":
            stack.append(i)
        elif ch == ")" and stack:
            start = stack.pop()
            out.append(text[start + 1:i])
    return out


def top_level_items(group: str) -> list[str]:
    """`group` split on the commas outside its nested parentheses."""
    items: list[str] = []
    cur: list[str] = []
    depth = 0
    for ch in group:
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        if ch == "," and depth == 0:
            items.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    items.append("".join(cur))
    return [i.strip() for i in items]


def cited_codes(line: str, codes: set[str]) -> set[str]:
    out: set[str] = set()
    for g in groups(strip_code_spans(line)):
        for item in top_level_items(g):
            m = re.match(r"[A-Z]+[0-9]+(?![A-Za-z0-9])", item)
            if m and m.group(0) in codes:
                out.add(m.group(0))
    return out


def citations(roadmap_text: str, codes: set[str]) -> dict[str, list[gatelib.Box]]:
    """Code -> the open boxes before `## Phase 11:` that cite it."""
    end = next((n for n, raw in enumerate(roadmap_text.splitlines(), start=1)
                if gatelib.CLOSED_SCOPE_END.match(raw)), None)
    out: dict[str, list[gatelib.Box]] = {c: [] for c in codes}
    for b in gatelib.parse_boxes(roadmap_text):
        if b.ticked or (end is not None and b.line >= end):
            continue
        for c in cited_codes(b.text, codes):
            out[c].append(b)
    return out


def plan_problem(text: str) -> str | None:
    """Why an open code's plan lacks its `**ROADMAP:**` line, or None."""
    for raw in text.splitlines():
        if raw.startswith("|"):
            break
        if raw.startswith("**ROADMAP:**"):
            if "§" not in raw or "Superseded here:" not in raw:
                return "its **ROADMAP:** line names no § section or lacks `Superseded here:`"
            return None
    return "no **ROADMAP:** line above its first table"


def check(
    rows: list[Row],
    cites: dict[str, list[gatelib.Box]],
    read_plan: Callable[[str], str | None],
    closed: bool = False,
) -> list[str]:
    problems: list[str] = []
    for r in rows:
        where = f"README.md:{r.line}: {r.code}"
        boxes = cites.get(r.code, [])
        lines = ", ".join(f"L{b.line}" for b in boxes)
        if not STATUS.match(r.status):
            problems.append(f"{where}: Status {r.status!r} is not proposed, in progress (#N), "
                            "implemented (#N), declined: <reason>, or superseded: <what>")
        plan = read_plan(r.plan)
        if plan is None:
            problems.append(f"{where}: plan {r.plan} is missing")
        if r.state in OPEN_STATES:
            if not boxes:
                problems.append(f"{where}: {r.state} but no open box in Phases 0 to 10 cites it")
            if plan is not None:
                why = plan_problem(plan)
                if why:
                    problems.append(f"{where}: plan {r.plan}: {why}")
            if closed:
                problems.append(f"{where}: still {r.status}")
        elif r.state and boxes:
            problems.append(f"{where}: {r.state} while open boxes cite it ({lines})")
        for b in boxes:
            later = [m for m, _ in lands_in_sections(b.text) if m > LAST_PHASE]
            if later:
                problems.append(f"{where}: citing box L{b.line} is deferred past Phase "
                                f"{LAST_PHASE} (lands in §{later[0]}.x)")
    return problems


def main(argv: list[str] | None = None, root: Path = ROOT) -> int:
    ap = argparse.ArgumentParser(prog="check_issues.py", description=__doc__.split("\n")[0])
    ap.add_argument("--closed", action="store_true",
                    help="also fail on any proposed or in progress row")
    args = ap.parse_args(argv)
    issues = root / "docs" / "reviews" / "issues"
    rows = parse_index((issues / "README.md").read_text(encoding="utf-8"))
    roadmap = (root / "docs" / "ROADMAP.md").read_text(encoding="utf-8")
    cites = citations(roadmap, {r.code for r in rows})

    def read_plan(name: str) -> str | None:
        p = issues / name
        if "/" in name or not p.is_file():
            return None
        return p.read_text(encoding="utf-8")

    problems = check(rows, cites, read_plan, args.closed)
    for p in problems:
        print(f"check_issues: {p}", file=sys.stderr)
    if problems:
        return 1
    print(f"check_issues: ok ({len(rows)} codes{', all closed' if args.closed else ''})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
