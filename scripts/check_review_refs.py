#!/usr/bin/env python3
"""Kernel review traceability: every finding in docs/reviews/KERNEL_REVIEW.md
is cited by id (F001..F999) in docs/ROADMAP.md, and every cited id exists.

`--closed` also fails while a CRITICAL or HIGH finding without a LATENT tag
is cited by an open box in Phases 0 to 10 (the lines before `## Phase 11:`).
Later phases may cite such a finding as a cross-reference. The Phase 10 gate
map runs that mode.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REVIEW = ROOT / "docs" / "reviews" / "KERNEL_REVIEW.md"
ROADMAP = ROOT / "docs" / "ROADMAP.md"

HEADING = re.compile(r"^#### (F\d{3}) · ")
SEVERITY = re.compile(r"^\*\*Severity:\*\* (CRITICAL|HIGH|MEDIUM|LOW)\b(.*)$")
FID = re.compile(r"\bF\d{3}\b")
BOX = re.compile(r"^\s*- \[( |x)\] ")
CLOSED_SCOPE_END = re.compile(r"^## Phase 11:")


@dataclass(frozen=True)
class Finding:
    fid: str
    severity: str
    latent: bool


@dataclass(frozen=True)
class Citation:
    line: int
    fid: str
    box: str | None  # "open", "closed", or None when the line is not a box
    phase10: bool  # the line is before "## Phase 11:"


def parse_review(text: str) -> dict[str, Finding]:
    """Findings keyed by id. The severity line follows its heading."""
    found: dict[str, Finding] = {}
    current: str | None = None
    for raw in text.splitlines():
        m = HEADING.match(raw)
        if m:
            current = m.group(1)
            continue
        if current is None:
            continue
        s = SEVERITY.match(raw)
        if s:
            found[current] = Finding(current, s.group(1), "LATENT" in s.group(2))
            current = None
    return found


def parse_roadmap(text: str) -> list[Citation]:
    cites: list[Citation] = []
    in_scope = True
    for n, raw in enumerate(text.splitlines(), start=1):
        if CLOSED_SCOPE_END.match(raw):
            in_scope = False
        ids = FID.findall(raw)
        if not ids:
            continue
        b = BOX.match(raw)
        state = None if b is None else ("closed" if b.group(1) == "x" else "open")
        cites.extend(Citation(n, fid, state, in_scope) for fid in ids)
    return cites


def check(
    findings: dict[str, Finding], cites: list[Citation], closed: bool
) -> list[str]:
    errors: list[str] = []
    cited = {c.fid for c in cites}
    for fid in sorted(findings):
        if fid not in cited:
            errors.append(f"{fid}: not cited in docs/ROADMAP.md")
    for c in cites:
        if c.fid not in findings:
            errors.append(f"ROADMAP.md:{c.line}: {c.fid} is not a finding in KERNEL_REVIEW.md")
    if closed:
        for c in cites:
            f = findings.get(c.fid)
            if not c.phase10 or f is None or f.latent or f.severity not in ("CRITICAL", "HIGH"):
                continue
            if c.box == "open":
                errors.append(f"ROADMAP.md:{c.line}: {c.fid} ({f.severity}) cited by an open box")
    return errors


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--closed", action="store_true", help="fail on open boxes in Phases "
                    "0 to 10 citing CRITICAL or HIGH findings without a LATENT tag")
    args = ap.parse_args(argv)
    findings = parse_review(REVIEW.read_text(encoding="utf-8"))
    if not findings:
        print(f"check_review_refs: no findings parsed from {REVIEW}", file=sys.stderr)
        return 1
    cites = parse_roadmap(ROADMAP.read_text(encoding="utf-8"))
    errors = check(findings, cites, args.closed)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"check_review_refs: ok ({len(findings)} findings)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
