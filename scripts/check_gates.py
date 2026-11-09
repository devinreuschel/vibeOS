#!/usr/bin/env python3
"""Gate maps (ROADMAP §10.9, the gate-map and `make gate` boxes).

`tests/gates/phase-<N>.toml` gives each exit-gate line of phase N, keyed by
its text, the entries that prove it (C-GATEMAP):

    [[line]]
    key = \"\"\"<the gate line after `- [ ] ` or `- [x] `>\"\"\"
    [[line.entry]]
    cmd = "make check"                               # a local command
    [[line.entry]]
    job = { workflow = "smp-stress.yml", job = "stress" }   # a hosted CI job
    [[line.entry]]
    record = { cmd = "make models" }                 # a dev-host record
    expect = "fail"                                  # optional

A key is compared with the gate line with whitespace collapsed. This script,
which `make check` runs, fails when:

- a `phase-<N>.toml` exists with N below 10 (Phases 0 to 9 get no map);
- a key matches no exit-gate line of phase N, or matches the tag line, or two
  keys match one line;
- a gate line other than the tag has no entry;
- an entry holds none or more than one of `cmd`, `job`, `record`, or an
  `expect` other than `"fail"`;
- an entry runs `make gate` or `scripts/gate.py`;
- a line names a `scripts/check_<x>.py` and no `cmd` or `record` entry of it
  contains that path;
- a job entry names a workflow that has a `self-hosted` label.

From Phase 11, `tests/gates/common.toml` holds named entries (not gate-line
keys). This script checks that each names a `make` target that exists.

It reads text only: no entry runs, and no `gh` or `ci-history` is needed.
`scripts/gate.py` runs the entries and imports `validate`, `load_map`,
`gate_lines`, `strip_code_spans`, and `lands_in_sections` from here, and so
does `scripts/check_issues.py`.

Standard library only.
"""

from __future__ import annotations

import re
import sys
import tomllib
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROADMAP = ROOT / "docs" / "ROADMAP.md"
GATES = ROOT / "tests" / "gates"
WORKFLOWS = ROOT / ".github" / "workflows"

MAP_FILE = re.compile(r"^phase-(\d+)\.toml$")
FIRST_MAPPED_PHASE = 10
COMMON = "common.toml"
MAKE_GOAL = re.compile(r"\bmake\b(?:\s+[A-Za-z_][A-Za-z0-9_]*=\S+)*\s+([A-Za-z0-9_.+/-]+)")
MAKE_TARGET = re.compile(r"^([A-Za-z0-9_.+/-]+):(?!=)", re.M)
PHASE = re.compile(r"^## Phase (\d+):")
EXIT_GATE = "**Exit gate**"
BOX = re.compile(r"^- \[( |x)\] ")
TAG = re.compile(r"^tag `phase-\d+`")
CODE_SPAN = re.compile(r"`[^`]*`")
LANDS_IN = re.compile(r"lands in ((?:§\d+\.\d+(?:\s*,?\s*(?:and|or)?\s*)?)+)")
SECTION_REF = re.compile(r"§(\d+)\.(\d+)")
CHECK_SCRIPT = re.compile(r"scripts/check_[A-Za-z0-9_]+\.py")
RUNS_GATE = re.compile(r"\bmake\b(?:\s+\S+)*?\s+gate(?:\s|$)|scripts/gate\.py")
KINDS = ("cmd", "job", "record")


class MapError(Exception):
    """A gate map is not in C-GATEMAP's shape."""


def norm(s: str) -> str:
    """`s` with every run of whitespace collapsed to one space, and trimmed."""
    return " ".join(s.split())


def strip_code_spans(s: str) -> str:
    """`s` with each backtick pair, left to right, and what it holds replaced
    by one space. An unpaired backtick stays."""
    return CODE_SPAN.sub(" ", s)


def lands_in_sections(text: str) -> list[tuple[int, int]]:
    """Every `§N.M` a `lands in` note of `text` lists, outside code spans:
    `lands in §10.2 and §10.7.` gives [(10, 2), (10, 7)]."""
    out: list[tuple[int, int]] = []
    for m in LANDS_IN.finditer(strip_code_spans(text)):
        out += [(int(a), int(b)) for a, b in SECTION_REF.findall(m.group(1))]
    return out


@dataclass(frozen=True)
class GateLine:
    line: int  # 1-based line number in the roadmap
    ticked: bool
    text: str  # after `- [ ] ` or `- [x] `
    tag: bool


def gate_lines(roadmap_text: str, phase: int) -> list[GateLine]:
    """The `- [ ]` and `- [x]` lines between phase N's `**Exit gate**` and its
    first `### ` heading."""
    out: list[GateLine] = []
    in_phase = in_gate = False
    for n, raw in enumerate(roadmap_text.splitlines(), start=1):
        p = PHASE.match(raw)
        if p:
            in_phase, in_gate = int(p.group(1)) == phase, False
            continue
        if not in_phase:
            continue
        if raw.startswith("#"):
            if in_gate or raw.startswith(("# ", "## ", "### ")):
                break
            continue
        if raw.strip() == EXIT_GATE:
            in_gate = True
            continue
        b = BOX.match(raw) if in_gate else None
        if b:
            text = raw[b.end():]
            out.append(GateLine(n, b.group(1) == "x", text, TAG.match(text) is not None))
    return out


@dataclass(frozen=True)
class Entry:
    kind: str  # "cmd", "job" or "record"
    cmd: str  # the command of a cmd or record entry; "" for a job
    workflow: str  # a job entry's workflow file; "" otherwise
    job: str  # a job entry's job id; "" otherwise
    expect_fail: bool

    def describe(self) -> str:
        if self.kind == "job":
            return f"job {self.workflow}:{self.job}"
        lead = "cmd" if self.kind == "cmd" else "record"
        return f"{lead} {self.cmd}" + (" (expect fail)" if self.expect_fail else "")


@dataclass(frozen=True)
class MapLine:
    key: str
    entries: tuple[Entry, ...]


def _str_field(v: object, where: str) -> str:
    if not isinstance(v, str) or not v.strip():
        raise MapError(f"{where} is not a non-empty string")
    return v


def _entry(raw: object, where: str) -> Entry:
    if not isinstance(raw, dict):
        raise MapError(f"{where} is not a table")
    extra = sorted(set(raw) - {*KINDS, "expect"})
    if extra:
        raise MapError(f"{where}: unknown field {', '.join(extra)}")
    kinds = [k for k in KINDS if k in raw]
    if len(kinds) != 1:
        raise MapError(f"{where} holds {len(kinds)} of cmd, job, record; it needs exactly one")
    expect = raw.get("expect")
    if expect is not None and expect != "fail":
        raise MapError(f"{where}: expect is {expect!r}; it is absent or \"fail\"")
    kind = kinds[0]
    if kind == "cmd":
        return Entry("cmd", _str_field(raw["cmd"], f"{where}: cmd"), "", "", expect == "fail")
    body = raw[kind]
    if not isinstance(body, dict):
        raise MapError(f"{where}: {kind} is not an inline table")
    if kind == "record":
        if set(body) != {"cmd"}:
            raise MapError(f"{where}: record holds only cmd")
        return Entry("record", _str_field(body["cmd"], f"{where}: record.cmd"), "", "",
                     expect == "fail")
    if set(body) != {"workflow", "job"}:
        raise MapError(f"{where}: job holds workflow and job")
    if expect is not None:
        raise MapError(f"{where}: a job entry takes no expect")
    return Entry("job", "", _str_field(body["workflow"], f"{where}: job.workflow"),
                 _str_field(body["job"], f"{where}: job.job"), False)


def load_map(text: str, name: str = "<map>") -> list[MapLine]:
    """The lines of one gate map. Raises `MapError` on anything outside
    C-GATEMAP's shape, and on two keys equal after whitespace is collapsed."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise MapError(f"{name}: {e}") from e
    extra = sorted(set(data) - {"line"})
    if extra:
        raise MapError(f"{name}: unknown table {', '.join(extra)}")
    raw_lines = data.get("line", [])
    if not isinstance(raw_lines, list):
        raise MapError(f"{name}: `line` is not an array of tables")
    out: list[MapLine] = []
    seen: dict[str, int] = {}
    for i, raw in enumerate(raw_lines, start=1):
        where = f"{name}: [[line]] {i}"
        if not isinstance(raw, dict):
            raise MapError(f"{where} is not a table")
        extra = sorted(set(raw) - {"key", "entry"})
        if extra:
            raise MapError(f"{where}: unknown field {', '.join(extra)}")
        key = norm(_str_field(raw.get("key"), f"{where}: key"))
        if key in seen:
            raise MapError(f"{where}: duplicate key (as [[line]] {seen[key]})")
        seen[key] = i
        entries = raw.get("entry", [])
        if not isinstance(entries, list):
            raise MapError(f"{where}: `entry` is not an array of tables")
        out.append(MapLine(key, tuple(
            _entry(e, f"{where} entry {j}") for j, e in enumerate(entries, start=1)
        )))
    return out


def workflow_self_hosted(yaml_text: str) -> bool:
    """`self-hosted` on any line of a workflow, outside comments: a `runs-on`
    label, a `labels:` list, or a matrix value alike."""
    for raw in yaml_text.splitlines():
        line = re.sub(r"(^|\s)#.*$", "", raw)
        if "self-hosted" in line:
            return True
    return False


def validate(
    phase: int,
    lines: list[MapLine],
    gates: list[GateLine],
    read_workflow: Callable[[str], str | None],
) -> list[str]:
    """Every map rule for phase N's map `lines` against its gate lines.
    `read_workflow(name)` returns the text of `.github/workflows/<name>`, or
    None when there is none. One message per problem."""
    problems: list[str] = []
    if phase < FIRST_MAPPED_PHASE:
        return [f"phase {phase}: Phases 0 to {FIRST_MAPPED_PHASE - 1} get no gate map"]
    by_text = {norm(g.text): g for g in gates}
    mapped: dict[int, MapLine] = {}
    for ml in lines:
        g = by_text.get(ml.key)
        if g is None:
            problems.append(f"key matches no exit-gate line of phase {phase}: {ml.key[:80]}")
            continue
        if g.tag:
            problems.append(f"key matches the tag line L{g.line}; the tag has no entry")
            continue
        if g.line in mapped:
            problems.append(f"two keys match L{g.line}")
        mapped[g.line] = ml
        for e in ml.entries:
            if e.cmd and RUNS_GATE.search(e.cmd):
                problems.append(f"L{g.line}: an entry runs make gate itself: {e.cmd}")
            if e.kind == "job":
                text = read_workflow(e.workflow)
                if text is not None and workflow_self_hosted(text):
                    problems.append(
                        f"L{g.line}: job {e.workflow}:{e.job} names a workflow with a "
                        "self-hosted label; gate jobs run on GitHub-hosted runners"
                    )
        runs = [e.cmd for e in ml.entries if e.kind in ("cmd", "record")]
        for script in sorted(set(CHECK_SCRIPT.findall(g.text))):
            if not any(script in c for c in runs):
                problems.append(f"L{g.line} names {script} and no cmd or record entry runs it")
    for g in gates:
        if g.tag:
            continue
        got = mapped.get(g.line)
        if got is None or not got.entries:
            problems.append(f"L{g.line} has no entry: {g.text[:80]}")
    return problems


def read_workflow_file(root: Path) -> Callable[[str], str | None]:
    def read(name: str) -> str | None:
        p = root / ".github" / "workflows" / name
        if "/" in name or not p.is_file():
            return None
        return p.read_text(encoding="utf-8")

    return read


def makefile_targets(text: str) -> set[str]:
    """Recipe names in a Makefile, continuation lines joined."""
    return set(MAKE_TARGET.findall(text.replace("\\\n", " ")))


def make_goal(cmd: str) -> str | None:
    """The last `make` goal in `cmd`, skipping `VAR=val` words."""
    found = None
    for m in MAKE_GOAL.finditer(cmd):
        goal = m.group(1)
        if "=" not in goal:
            found = goal
    return found


@dataclass(frozen=True)
class CommonEntry:
    name: str
    cmd: str


def load_common(text: str, name: str = "<common>") -> list[CommonEntry]:
    """Named `[[entry]]` rows of `common.toml`."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise MapError(f"{name}: {e}") from e
    extra = sorted(set(data) - {"entry"})
    if extra:
        raise MapError(f"{name}: unknown table {', '.join(extra)}")
    raw = data.get("entry", [])
    if not isinstance(raw, list):
        raise MapError(f"{name}: `entry` is not an array of tables")
    out: list[CommonEntry] = []
    seen: dict[str, int] = {}
    for i, row in enumerate(raw, start=1):
        where = f"{name}: [[entry]] {i}"
        if not isinstance(row, dict):
            raise MapError(f"{where} is not a table")
        extra = sorted(set(row) - {"name", "cmd"})
        if extra:
            raise MapError(f"{where}: unknown field {', '.join(extra)}")
        key = _str_field(row.get("name"), f"{where}: name")
        if key in seen:
            raise MapError(f"{where}: duplicate name (as [[entry]] {seen[key]})")
        seen[key] = i
        out.append(CommonEntry(key, _str_field(row.get("cmd"), f"{where}: cmd")))
    return out


def validate_common(entries: list[CommonEntry], targets: set[str], name: str) -> list[str]:
    """Each common entry names a Makefile target that exists."""
    problems: list[str] = []
    for e in entries:
        goal = make_goal(e.cmd)
        if goal is None:
            problems.append(f"{name}: {e.name}: cmd names no make target: {e.cmd}")
        elif goal not in targets:
            problems.append(f"{name}: {e.name}: make target {goal!r} does not exist")
    return problems


def map_files(gates_dir: Path) -> list[tuple[int, Path]]:
    """(N, path) of every `phase-<N>.toml`, by N; never a `-needs` file."""
    out = []
    for p in gates_dir.glob("phase-*.toml"):
        m = MAP_FILE.match(p.name)
        if m:
            out.append((int(m.group(1)), p))
    return sorted(out)


def main(argv: list[str] | None = None, root: Path = ROOT) -> int:
    del argv
    roadmap = (root / "docs" / "ROADMAP.md").read_text(encoding="utf-8")
    problems: list[str] = []
    counts: list[str] = []
    for phase, path in map_files(root / "tests" / "gates"):
        rel = path.relative_to(root)
        try:
            lines = load_map(path.read_text(encoding="utf-8"), str(rel))
        except MapError as e:
            problems.append(str(e))
            continue
        found = validate(phase, lines, gate_lines(roadmap, phase), read_workflow_file(root))
        problems += [f"{rel}: {p}" for p in found]
        counts.append(f"phase {phase}: {len(lines)} lines")
    common = root / "tests" / "gates" / COMMON
    if common.is_file():
        rel = common.relative_to(root)
        makefile = ""
        mk = root / "Makefile"
        if mk.is_file():
            makefile = mk.read_text(encoding="utf-8")
        try:
            entries = load_common(common.read_text(encoding="utf-8"), str(rel))
        except MapError as e:
            problems.append(str(e))
        else:
            problems += validate_common(entries, makefile_targets(makefile), str(rel))
            counts.append(f"common: {len(entries)} entries")
    for p in problems:
        print(f"check_gates: {p}", file=sys.stderr)
    if problems:
        return 1
    print(f"check_gates: ok ({', '.join(counts) or 'no gate maps'})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
