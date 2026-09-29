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

Placement, as the tracking rule in KERNEL_REVIEW.md and Phase 10's preamble place
a finding's boxes. A placement citation is a finding id on a box line with a phase
(`gatelib.roadmap_boxes`; exit-gate and Stretch lines count as boxes of their
phase, and the `- [ ]` lines after `# Beyond` are proposals, not boxes). A box line
that names `check_review_refs.py` states this script's rules, so its ids are
examples, not citations. Every finding has a placement citation; a CRITICAL or HIGH
finding without a LATENT tag has one in Phase 10 or earlier; and a LATENT finding
whose tag names a phase has one in that phase or an earlier one, except the ids
DEPARTURES lists. The tag is the text of `LATENT (...)` up to its matching
parenthesis, since tags nest parentheses. Its phases are the N of each `Phase N`,
`§N`, and `§N.M` in the tag, and the smallest is its milestone ("Phase 12.5 ..."
is 12, "Phase 18/22" is 18, "ROADMAP §20.1" is 20). A tag that names no phase gets
the first rule only. A DEPARTURES entry fails when its key names no box or several,
when that box does not cite the id, or when the finding passes without it. The
bare mode (no option) runs these rules.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402
from scripts.gatelib import (  # noqa: E402
    BOX,
    CLOSED_SCOPE_END,
    FID,
    GATES,
    HEADING,
    REVIEW,
    ROADMAP,
    SEVERITY,
    Box,
    GateError,
    NeedsFile,
    load_all_needs,
    match_key,
    parse_boxes,
    wave1_lines,
)

__all__ = [
    "DEPARTURES", "Citation", "Finding", "check", "check_needs", "check_placement",
    "parse_review", "parse_roadmap", "tag_phase", "wave",
]

# A lands clause in box text: "lands after", "land before", "lands with or after".
LANDS = re.compile(r"\blands? (after|with|before)\b", re.IGNORECASE)

# A phase a LATENT tag names: `Phase N`, `§N`, or `§N.M`.
TAG_PHASE = re.compile(r"(?:\bPhase\s+|§)(\d+)")
OWN_RULES = "check_review_refs.py"

# Findings whose box sits after the phase their LATENT tag names: id -> (a key naming
# the box, the reason that box gives).
DEPARTURES: dict[str, tuple[str, str]] = {
    "F122": (
        "the virtio drivers meet the virtio 1.2 driver requirements",
        "§26.4: Firecracker's and cloud-hypervisor's devices are the first non-QEMU virtio "
        "devices the drivers meet",
    ),
}


@dataclass(frozen=True)
class Finding(gatelib.Finding):
    tag: str | None = None  # the text of `LATENT (...)`, None without one


def latent_tag(rest: str) -> str | None:
    """The text inside `LATENT (...)` in a severity line's tail, up to the matching
    parenthesis."""
    at = rest.find("LATENT (")
    if at < 0:
        return None
    start = at + len("LATENT (")
    depth = 1
    for i in range(start, len(rest)):
        if rest[i] == "(":
            depth += 1
        elif rest[i] == ")":
            depth -= 1
            if depth == 0:
                return rest[start:i]
    return rest[start:]


def parse_review(text: str) -> dict[str, Finding]:
    """`gatelib.parse_review`'s findings, each with its LATENT tag."""
    tags: dict[str, str | None] = {}
    current: str | None = None
    for raw in text.splitlines():
        m = HEADING.match(raw)
        if m:
            current = m.group(1)
            continue
        s = SEVERITY.match(raw) if current is not None else None
        if current is not None and s:
            tags[current] = latent_tag(s.group(2))
            current = None
    return {
        fid: Finding(f.fid, f.severity, f.latent, tags.get(fid))
        for fid, f in gatelib.parse_review(text).items()
    }


def tag_phase(tag: str | None) -> int | None:
    """The smallest phase a LATENT tag names, or None."""
    if tag is None:
        return None
    phases = [int(n) for n in TAG_PHASE.findall(tag)]
    return min(phases) if phases else None


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


def check_placement(findings: dict[str, Finding], boxes: list[Box]) -> list[str]:
    """The placement rules and DEPARTURES (the module docstring states them)."""
    errors: list[str] = []
    first: dict[str, Box] = {}
    for b in boxes:
        if b.phase is None or OWN_RULES in b.text:
            continue
        for fid in FID.findall(b.text):
            old = first.get(fid)
            if old is None or b.phase < (old.phase or 0):
                first[fid] = b

    def passes_latent(f: Finding) -> bool:
        milestone = tag_phase(f.tag) if f.latent else None
        b = first.get(f.fid)
        return milestone is None or (b is not None and (b.phase or 0) <= milestone)

    for fid in sorted(findings):
        f = findings[fid]
        at = first.get(fid)
        if at is None:
            errors.append(f"{fid}: cited by no box line, only by prose")
            continue
        if not f.latent and f.severity in ("CRITICAL", "HIGH") and (at.phase or 0) > 10:
            errors.append(f"{fid} ({f.severity}): no box before `## Phase 11:` cites it "
                          f"(first ROADMAP.md:{at.line}, Phase {at.phase})")
        if fid not in DEPARTURES and not passes_latent(f):
            errors.append(f"{fid}: LATENT tag names Phase {tag_phase(f.tag)}, first box "
                          f"ROADMAP.md:{at.line} is in Phase {at.phase}")
    for fid, (key, _reason) in sorted(DEPARTURES.items()):
        try:
            box = match_key(key, boxes)
        except GateError as e:
            errors.append(f"DEPARTURES {fid}: {e}")
            continue
        if fid not in FID.findall(box.text):
            errors.append(f"DEPARTURES {fid}: ROADMAP.md:{box.line} does not cite {fid}")
        found = findings.get(fid)
        if found is None or passes_latent(found):
            errors.append(f"DEPARTURES {fid}: stale, the finding passes without the entry")
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
    bare = not args.closed and args.wave is None and args.print_wave is None
    errors += check_placement(findings, parse_boxes(roadmap_text)) if bare else []
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
