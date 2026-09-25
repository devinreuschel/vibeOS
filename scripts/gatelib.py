"""Shared readers for the gate scripts (ROADMAP §10.9).

`check_review_refs.py`, `check_ticks.py`, `check_gone.py`, and the later gate
scripts import this module rather than parse `docs/ROADMAP.md`, the review,
the needs files, commit messages, runs, or results files themselves.

- `parse_boxes` / `roadmap_boxes`: the checkbox lines of the roadmap, with the
  phase and section each sits in.
- `match_key`: a needs-file key names exactly one box line.
- `load_needs` / `load_all_needs`: `tests/gates/phase-<N>-needs.toml`.
- `wave1_lines`: the line numbers of the wave-1 boxes.
- `parse_review`: KERNEL_REVIEW.md's findings and their severities.
- `pr_commits`, `message_lines`, `docs_only`: git readers. Each takes `repo`.
- `run_proves_commit`, `run_counts_for_pr`: which CI runs prove which commit.
- `load_results`, `parse_bracket`: harness results files and bracketed proofs.

Standard library only.
"""

from __future__ import annotations

import json
import re
import subprocess
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
REVIEW = ROOT / "docs" / "reviews" / "KERNEL_REVIEW.md"
ROADMAP = ROOT / "docs" / "ROADMAP.md"
GATES = ROOT / "tests" / "gates"

HEADING = re.compile(r"^#### (F\d{3}) · ")
SEVERITY = re.compile(r"^\*\*Severity:\*\* (CRITICAL|HIGH|MEDIUM|LOW)\b(.*)$")
FID = re.compile(r"\bF\d{3}\b")
BOX = re.compile(r"^\s*- \[( |x)\] ")
CLOSED_SCOPE_END = re.compile(r"^## Phase 11:")
PHASE = re.compile(r"^## Phase (\d+):")
SECTION = re.compile(r"^### (\d+\.\d+)\b")
TOP_HEADING = re.compile(r"^#{1,2} ")
NEEDS_FILE = re.compile(r"^phase-(\d+)-needs\.toml$")
ROW_FIELDS = frozenset({"key", "needs", "closes", "why"})

# C-TICK's brackets: the scheduled workflow or record that runs a proof every
# per-push tier skips. None: read from `ci-history` records only.
BRACKETS: dict[str, str | None] = {
    "nightly": "nightly.yml",
    "weekly": "smp-stress.yml",
    "macos": "macos.yml",
    "release": "release.yml",
    "dev-host": None,
    "ci-history": None,
}
BRACKET = re.compile(r"\s\[([A-Za-z0-9-]+)\]$")

# Events whose run tests its own head SHA (ROADMAP §10.9, the dev-host box).
PROVING_EVENTS = frozenset({"push", "schedule", "workflow_dispatch"})


class GateError(Exception):
    """A gate input is malformed or a key does not name one box."""


@dataclass(frozen=True)
class Finding:
    fid: str
    severity: str
    latent: bool


@dataclass(frozen=True)
class Box:
    line: int
    ticked: bool
    text: str  # the line after its `- [ ] ` or `- [x] ` marker
    section: str | None  # "10.9" under `### 10.9 ...`
    phase: int | None  # N under `## Phase N:`; None elsewhere, `# Beyond` included


@dataclass(frozen=True)
class NeedRow:
    key: str
    needs: tuple[str, ...]
    closes: tuple[str, ...]
    why: str


NeedsFile = tuple[int, list[NeedRow], list[str]]


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


def parse_boxes(text: str) -> list[Box]:
    """Every checkbox line of a roadmap text, a git blob's included."""
    boxes: list[Box] = []
    phase: int | None = None
    section: str | None = None
    for n, raw in enumerate(text.splitlines(), start=1):
        p = PHASE.match(raw)
        if p:
            phase, section = int(p.group(1)), None
            continue
        if TOP_HEADING.match(raw):
            phase, section = None, None
            continue
        if raw.startswith("### "):
            s = SECTION.match(raw)
            section = s.group(1) if s else None
            continue
        b = BOX.match(raw)
        if b:
            boxes.append(Box(n, b.group(1) == "x", raw[b.end():], section, phase))
    return boxes


def roadmap_boxes(path: Path = ROADMAP) -> list[Box]:
    return parse_boxes(path.read_text(encoding="utf-8"))


def match_key(key: str, boxes: list[Box], lines: list[str] | None = None) -> Box:
    """The one box `key` names. With `lines` (the roadmap's lines), `key` must
    be a substring of exactly one line, and that line must be a box."""
    by_line = {b.line: b for b in boxes}
    if lines is not None:
        hits = [n for n, raw in enumerate(lines, start=1) if key in raw]
    else:
        hits = [b.line for b in boxes if key in b.text]
    if not hits:
        raise GateError(f"key {key!r} matches no line")
    if len(hits) > 1:
        where = ", ".join(f"L{n}" for n in hits[:5])
        raise GateError(f"key {key!r} matches {len(hits)} lines ({where})")
    box = by_line.get(hits[0])
    if box is None:
        raise GateError(f"key {key!r} matches L{hits[0]}, which is not a box")
    return box


def _str_list(v: object, what: str) -> tuple[str, ...]:
    if not isinstance(v, list) or not all(isinstance(x, str) for x in v):
        raise GateError(f"{what} is not a list of strings")
    return tuple(v)


def parse_needs(text: str, name: str = "<needs>") -> tuple[list[NeedRow], list[str]]:
    """Rows and `[wave1].roots` of one needs file. Rows keyed on one box by
    the same key merge: their lists are unioned, in order."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise GateError(f"{name}: {e}") from e
    unknown = sorted(set(data) - {"box", "wave1"})
    if unknown:
        raise GateError(f"{name}: unknown table {', '.join(unknown)}")
    roots: list[str] = []
    w = data.get("wave1", {})
    if not isinstance(w, dict) or set(w) - {"roots"}:
        raise GateError(f"{name}: [wave1] holds only `roots`")
    if "roots" in w:
        roots = list(_str_list(w["roots"], f"{name}: [wave1].roots"))
    raw_rows = data.get("box", [])
    if not isinstance(raw_rows, list):
        raise GateError(f"{name}: `box` is not an array of tables")
    merged: dict[str, NeedRow] = {}
    for i, r in enumerate(raw_rows, start=1):
        where = f"{name}: [[box]] {i}"
        if not isinstance(r, dict):
            raise GateError(f"{where} is not a table")
        extra = sorted(set(r) - ROW_FIELDS)
        if extra:
            raise GateError(f"{where}: unknown field {', '.join(extra)}")
        key = r.get("key")
        if not isinstance(key, str) or not key:
            raise GateError(f"{where}: no `key`")
        needs = _str_list(r.get("needs", []), f"{where}: `needs`")
        closes = _str_list(r.get("closes", []), f"{where}: `closes`")
        why = r.get("why", "")
        if not isinstance(why, str):
            raise GateError(f"{where}: `why` is not a string")
        old = merged.get(key)
        if old is not None:
            needs = old.needs + tuple(n for n in needs if n not in old.needs)
            closes = old.closes + tuple(c for c in closes if c not in old.closes)
            why = "; ".join(x for x in (old.why, why) if x)
        merged[key] = NeedRow(key, needs, closes, why)
    return list(merged.values()), roots


def load_needs(path: Path) -> tuple[list[NeedRow], list[str]]:
    return parse_needs(path.read_text(encoding="utf-8"), path.name)


def load_all_needs(directory: Path = GATES, errors: list[str] | None = None) -> list[NeedsFile]:
    """(phase from the file name, rows, roots) of every needs file, by phase.
    A malformed file raises `GateError`, or with `errors` is appended there
    and left out."""
    out: list[NeedsFile] = []
    for p in sorted(directory.glob("phase-*-needs.toml")):
        m = NEEDS_FILE.match(p.name)
        if m is None:
            continue
        try:
            rows, roots = load_needs(p)
        except GateError as e:
            if errors is None:
                raise
            errors.append(str(e))
            continue
        out.append((int(m.group(1)), rows, roots))
    out.sort(key=lambda t: t[0])
    return out


def needs_edges(needs: list[NeedsFile], boxes: list[Box], lines: list[str]) -> dict[int, set[int]]:
    """Box line -> the box lines it needs, over every needs file. Keys that
    do not name one box are left out; `check_review_refs.py` reports them."""
    edges: dict[int, set[int]] = {}
    for _, rows, _ in needs:
        for r in rows:
            try:
                k = match_key(r.key, boxes, lines).line
            except GateError:
                continue
            s = edges.setdefault(k, set())
            for n in r.needs:
                try:
                    s.add(match_key(n, boxes, lines).line)
                except GateError:
                    continue
    return edges


def wave1_lines(
    *,
    roadmap_text: str | None = None,
    review_text: str | None = None,
    needs: list[NeedsFile] | None = None,
) -> set[int]:
    """Wave 1: the boxes `check_review_refs.py --closed` inspects (before
    `## Phase 11:`, citing a CRITICAL or HIGH finding without LATENT, open or
    ticked) and the phase-10 `[wave1].roots`, closed over every `needs`."""
    if roadmap_text is None:
        roadmap_text = ROADMAP.read_text(encoding="utf-8")
    if review_text is None:
        review_text = REVIEW.read_text(encoding="utf-8")
    if needs is None:
        needs = load_all_needs()
    findings = parse_review(review_text)
    lines = roadmap_text.splitlines()
    boxes = parse_boxes(roadmap_text)
    by_line = {b.line: b for b in boxes}
    seeds: set[int] = set()
    for n, raw in enumerate(lines, start=1):
        if CLOSED_SCOPE_END.match(raw):
            break
        if n not in by_line:
            continue
        for fid in FID.findall(raw):
            f = findings.get(fid)
            if f is not None and not f.latent and f.severity in ("CRITICAL", "HIGH"):
                seeds.add(n)
    for phase, _, roots in needs:
        if phase != 10:
            continue
        for key in roots:
            try:
                seeds.add(match_key(key, boxes, lines).line)
            except GateError:
                continue
    edges = needs_edges(needs, boxes, lines)
    wave: set[int] = set()
    todo = list(seeds)
    while todo:
        n = todo.pop()
        if n in wave:
            continue
        wave.add(n)
        todo.extend(edges.get(n, ()))
    return wave


def git(repo: Path, *args: str, check: bool = True) -> str:
    """Run git in `repo` with no color, pager, or external diff."""
    r = subprocess.run(
        ["git", "-C", str(repo), "-c", "core.quotepath=off", *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if check and r.returncode != 0:
        raise GateError(f"git {' '.join(args)}: {r.stderr.strip()}")
    return r.stdout


def pr_commits(base: str, head: str, repo: Path = ROOT) -> list[str]:
    """The non-merge commits of `base..head`, oldest first."""
    out = git(repo, "rev-list", "--no-merges", "--reverse", f"{base}..{head}")
    return out.split()


def parse_message_lines(message: str, tag: str) -> list[str]:
    """The text after `<tag>: ` of every line that starts with it, wherever it
    sits in the message and however it is indented."""
    lead = f"{tag}: "
    out: list[str] = []
    for raw in message.splitlines():
        s = raw.strip()
        if s.startswith(lead):
            out.append(s[len(lead):])
    return out


def message_lines(sha: str, tag: str, repo: Path = ROOT) -> list[str]:
    return parse_message_lines(git(repo, "log", "-1", "--format=%B", sha), tag)


def docs_only(a: str, b: str, repo: Path = ROOT) -> bool:
    """The trees of `a` and `b` differ only under `docs/` and in `CHANGELOG.md`."""
    names = git(repo, "diff", "--no-renames", "--name-only", a, b).split("\n")
    return all(not p or p.startswith("docs/") or p == "CHANGELOG.md" for p in names)


def run_commit(run: dict[str, Any]) -> str:
    """The commit a run tested: the input commit a workflow that takes one
    records as `commit`, else the run's head SHA."""
    c = run.get("commit")
    if isinstance(c, str) and c:
        return c
    h = run.get("head_sha")
    return h if isinstance(h, str) else ""


def run_proves_commit(run: dict[str, Any], commit: str) -> bool:
    """ROADMAP §10.9 (the dev-host box): a run proves `commit` only when its
    event is `push`, `schedule`, or `workflow_dispatch` and its head SHA is
    `commit`, or, for a workflow that takes a commit as input, its CI-history
    record names `commit`. A `pull_request` run proves nothing."""
    if run.get("event") not in PROVING_EVENTS or not commit:
        return False
    return run_commit(run) == commit


def commit_counts_for_pr(
    sha: str,
    pr: list[str],
    head: str,
    *,
    merge_base: str | None = None,
    repo: Path = ROOT,
) -> bool:
    """`sha` is the head, a commit of the pull request whose tree differs from
    the head's only under `docs/` and in `CHANGELOG.md`, or the merge base when
    the head differs from it only there (ROADMAP §10.9, #93 §9 Q-04)."""
    if not sha:
        return False
    if sha == head:
        return True
    if sha in pr and docs_only(sha, head, repo):
        return True
    return merge_base is not None and sha == merge_base and docs_only(sha, head, repo)


def run_counts_for_pr(
    run: dict[str, Any],
    pr: list[str],
    head: str,
    *,
    merge_base: str | None = None,
    repo: Path = ROOT,
) -> bool:
    """A scheduled or dispatched run counts as a bracketed proof's run for a
    pull request (the `check_ticks.py` box): it proves the head, a docs-only
    commit of the pull request, or a docs-only merge base."""
    if run.get("event") not in PROVING_EVENTS:
        return False
    return commit_counts_for_pr(run_commit(run), pr, head, merge_base=merge_base, repo=repo)


def load_results(directory: Path) -> list[dict[str, Any]]:
    """Every results file (C-RESULTS, schema 1) under `directory`, nested ones
    included, as an artifact download nests them. Each dict gains `_path`."""
    out: list[dict[str, Any]] = []
    if not directory.is_dir():
        return out
    for p in sorted(directory.rglob("*.json")):
        try:
            d = json.loads(p.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as e:
            raise GateError(f"{p}: not a results file: {e}") from e
        if not isinstance(d, dict) or d.get("schema") != 1:
            raise GateError(f"{p}: not a schema-1 results file")
        d["_path"] = str(p)
        out.append(d)
    return out


def parse_bracket(proof: str) -> str | None:
    """The bracket a proof ends with (`name [nightly]`), or None."""
    m = BRACKET.search(proof.rstrip())
    if m is None:
        return None
    if m.group(1) not in BRACKETS:
        raise GateError(f"unknown bracket [{m.group(1)}] (one of {', '.join(BRACKETS)})")
    return m.group(1)
