#!/usr/bin/env python3
"""Kernel review traceability and the box dependency rows (ROADMAP §10.9).

Every finding in docs/reviews/KERNEL_REVIEW.md is cited by id (F001..F999) in
docs/ROADMAP.md, and every cited id exists.

Every tests/gates/phase-<N>-needs.toml loads: a row holds `key`, `needs`,
`closes`, and `why` only; every key, `needs`, `closes`, and `[wave1].roots`
entry is a substring of exactly one roadmap line, and that line is a box; no
row of phase N's file needs a box in a later phase or in none; and every box
in Phases 0 to 10 whose text has a lands-after, lands-with, or lands-before
clause is a row's key or appears in a row's `needs` or `closes`.

`--closed` also fails while a CRITICAL or HIGH finding without a LATENT tag
is cited by an open box in Phases 0 to 10 (the lines before `## Phase 11:`).
Later phases may cite such a finding as a cross-reference. The Phase 10 gate
map runs that mode.

`--wave 1` also fails while a wave-1 box is open: the boxes `--closed`
inspects and phase 10's `[wave1].roots`, with every box they need through the
rows of every needs file. `--print-wave 1` prints them, one per line.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts.gatelib import (  # noqa: E402
    BOX,
    CLOSED_SCOPE_END,
    FID,
    GATES,
    REVIEW,
    ROADMAP,
    Box,
    Finding,
    GateError,
    NeedsFile,
    load_all_needs,
    match_key,
    parse_boxes,
    parse_review,
    wave1_lines,
)

__all__ = [
    "Citation", "Finding", "check", "check_needs", "parse_review", "parse_roadmap", "wave",
]

# A lands clause in box text: "lands after", "land before", "lands with or after".
LANDS = re.compile(r"\blands? (after|with|before)\b", re.IGNORECASE)


@dataclass(frozen=True)
class Citation:
    line: int
    fid: str
    box: str | None  # "open", "closed", or None when the line is not a box
    phase10: bool  # the line is before "## Phase 11:"


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


def check_needs(roadmap_text: str, needs: list[NeedsFile]) -> list[str]:
    """The needs-file rules: keys, later-phase needs, and lands clauses."""
    errors: list[str] = []
    lines = roadmap_text.splitlines()
    boxes = parse_boxes(roadmap_text)
    rowed: set[int] = set()

    def resolve(where: str, key: str) -> Box | None:
        try:
            return match_key(key, boxes, lines)
        except GateError as e:
            errors.append(f"{where}: {e}")
            return None

    for phase, rows, roots in needs:
        name = f"phase-{phase}-needs.toml"
        for key in roots:
            resolve(f"{name} [wave1]", key)
        for r in rows:
            where = f"{name} row {r.key!r}"
            b = resolve(name, r.key)
            if b is not None:
                rowed.add(b.line)
            for n in r.needs:
                nb = resolve(f"{where} needs", n)
                if nb is None:
                    continue
                rowed.add(nb.line)
                if nb.phase is None or nb.phase > phase:
                    at = "no phase" if nb.phase is None else f"Phase {nb.phase}"
                    errors.append(f"{where}: needs L{nb.line}, a box in {at}, "
                                  f"after Phase {phase}")
            for c in r.closes:
                cb = resolve(f"{where} closes", c)
                if cb is not None:
                    rowed.add(cb.line)
    for b in boxes:
        if b.phase is None or b.phase > 10 or not LANDS.search(b.text):
            continue
        if b.line not in rowed:
            words = " ".join(b.text.split()[:8])
            errors.append(f"ROADMAP.md:{b.line}: a lands clause and no needs row: {words}")
    return errors


def wave(
    number: int, roadmap_text: str, review_text: str, needs: list[NeedsFile]
) -> list[Box]:
    """The boxes of wave `number`, by line. Only wave 1 is defined."""
    if number != 1:
        raise ValueError(f"wave {number}: only wave 1 is defined")
    lines = wave1_lines(roadmap_text=roadmap_text, review_text=review_text, needs=needs)
    return [b for b in parse_boxes(roadmap_text) if b.line in lines]


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--closed", action="store_true", help="fail on open boxes in Phases "
                    "0 to 10 citing CRITICAL or HIGH findings without a LATENT tag")
    ap.add_argument("--wave", type=int, choices=[1], help="fail while a box of this wave is open")
    ap.add_argument("--print-wave", type=int, choices=[1], help="print this wave's boxes")
    args = ap.parse_args(argv)
    review_text = REVIEW.read_text(encoding="utf-8")
    findings = parse_review(review_text)
    if not findings:
        print(f"check_review_refs: no findings parsed from {REVIEW}", file=sys.stderr)
        return 1
    roadmap_text = ROADMAP.read_text(encoding="utf-8")
    cites = parse_roadmap(roadmap_text)
    errors = check(findings, cites, args.closed)
    load_errors: list[str] = []
    needs = load_all_needs(GATES, load_errors)
    errors += load_errors + check_needs(roadmap_text, needs)
    if args.print_wave is not None:
        for b in wave(args.print_wave, roadmap_text, review_text, needs):
            mark = "[x]" if b.ticked else "[ ]"
            print(f"L{b.line} {mark} {' '.join(b.text.split()[:8])}")
    if args.wave is not None:
        for b in wave(args.wave, roadmap_text, review_text, needs):
            if not b.ticked:
                words = " ".join(b.text.split()[:8])
                errors.append(f"ROADMAP.md:{b.line}: wave {args.wave} box open: {words}")
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    if args.print_wave is None:
        print(f"check_review_refs: ok ({len(findings)} findings, "
              f"{sum(len(r) for _, r, _ in needs)} needs rows)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
