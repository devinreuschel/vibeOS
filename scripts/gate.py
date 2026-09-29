#!/usr/bin/env python3
"""The phase exit gate (ROADMAP §10.9, the gate-map and `make gate` boxes).

`run_gate` prints one row per exit-gate line of phase N, each followed by its
entries' results, and a final verdict:

    PASS  L1145  `make check` (fmt, clippy ...
          ok    cmd make check
    FAIL  L1147  `make test` passes on the scheduled macOS CI job ...
          FAIL  cmd test -f .github/workflows/macos.yml  exit 1 (build/gate/phase-10/3.log)
    TAG   L1176  tag `phase-10` and release `v0.10.0`
    gate: phase 10 at <sha>: fail

It runs every entry of `tests/gates/phase-<N>.toml` (C-GATEMAP;
`scripts/check_gates.py` holds the map's rules, which run first): a `cmd`
entry through `sh -c` at the root, each distinct command once. A line passes
only when every entry passes; an `expect = "fail"` entry passes on a non-zero
exit, and only after a plain entry of its line passed in the same run.

Standard library only.
"""

from __future__ import annotations

import fnmatch
import json
import os
import re
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import check_gates  # noqa: E402
from scripts.check_gates import Entry, GateLine, MapLine, norm  # noqa: E402

ARCH = "x86_64"
KTEST_VAR = re.compile(r"(?:^|\s)VIBEOS_KTEST=('[^']*'|\"[^\"]*\"|\S+)")
MAKE_TIER = re.compile(r"\bmake\s+(?:\S+=\S*\s+)*(test-[A-Za-z0-9-]+)")
ROW_TEXT = 100


# --- injected tools -----------------------------------------------------------


class Runner(Protocol):
    def __call__(self, cmd: str, cwd: Path, log: Path) -> int: ...


def shell_runner(cmd: str, cwd: Path, log: Path) -> int:
    """`sh -c cmd` in `cwd`, its output in `log`; the exit status."""
    log.parent.mkdir(parents=True, exist_ok=True)
    with open(log, "wb") as f:
        return subprocess.run(["sh", "-c", cmd], cwd=cwd, stdout=f, stderr=subprocess.STDOUT,
                              check=False).returncode



# --- entries ------------------------------------------------------------------


@dataclass(frozen=True)
class EntryResult:
    entry: Entry
    ok: bool
    detail: str = ""


def ktest_selection(cmd: str) -> tuple[list[str], str | None] | None:
    """The names a `VIBEOS_KTEST=` command selects and the tier it runs
    (`make test-kernel` gives `test-kernel`), or None without a selection."""
    m = KTEST_VAR.search(cmd)
    if m is None:
        return None
    value = m.group(1)
    if value[:1] in ("'", '"'):
        value = value[1:-1]
    names = [n.strip() for n in value.split(",") if n.strip()]
    t = MAKE_TIER.search(cmd)
    return names, (t.group(1) if t else None)


def results_file(root: Path, tier: str) -> Path:
    return root / "build" / "results" / f"{ARCH}-{tier}.json"


def check_selection(names: list[str], results: Path) -> str | None:
    """None when every literal name passed and every glob matched a passed
    name in the results file; else what is missing."""
    try:
        data = json.loads(results.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return f"no results file {results.name}"
    ktest = data.get("ktest") if isinstance(data, dict) else None
    passed = ktest.get("passed") if isinstance(ktest, dict) else None
    if not isinstance(passed, list):
        return f"{results.name} has no ktest.passed"
    have = {p for p in passed if isinstance(p, str)}
    missing = []
    for n in names:
        if any(c in n for c in "*?["):
            if not fnmatch.filter(sorted(have), n):
                missing.append(f"{n} matched no passed test")
        elif n not in have:
            missing.append(f"{n} did not pass")
    return "; ".join(missing) or None


class CmdCache:
    """Runs each distinct command once per gate run and logs it to
    `build/gate/phase-<N>/<i>.log`, `i` counting the map's entries from 1."""

    def __init__(self, root: Path, phase: int, runner: Runner) -> None:
        self.root = root
        self.logs = root / "build" / "gate" / f"phase-{phase}"
        self.runner = runner
        self.done: dict[str, tuple[int, str, Path]] = {}

    def run(self, cmd: str, index: int) -> tuple[int, str, Path]:
        """(exit status, selection problem or "", log) of `cmd`."""
        if cmd in self.done:
            return self.done[cmd]
        sel = ktest_selection(cmd)
        log = self.logs / f"{index}.log"
        if sel is not None and sel[1] is None:
            got = (1, "VIBEOS_KTEST names no `make test-*` tier", log)
            self.done[cmd] = got
            return got
        results = results_file(self.root, sel[1]) if sel is not None and sel[1] else None
        if results is not None:
            results.unlink(missing_ok=True)  # C-RESULTS unions into a stale file
        rc = self.runner(cmd, self.root, log)
        problem = ""
        if rc == 0 and sel is not None and results is not None:
            problem = check_selection(sel[0], results) or ""
        self.done[cmd] = (rc, problem, log)
        return self.done[cmd]


def eval_cmd(e: Entry, index: int, cache: CmdCache, root: Path) -> EntryResult:
    rc, problem, log = cache.run(e.cmd, index)
    where = os.path.relpath(log, root)
    if e.expect_fail:
        if rc != 0:
            return EntryResult(e, True, f"failed as expected, exit {rc} ({where})")
        return EntryResult(e, False, f"exit 0, expected a failure ({where})")
    if rc != 0:
        return EntryResult(e, False, f"exit {rc} ({where})")
    if problem:
        return EntryResult(e, False, f"{problem} ({where})")
    return EntryResult(e, True)


def line_verdict(results: list[EntryResult]) -> bool:
    """A line passes only when it has entries and every one passed."""
    return bool(results) and all(r.ok for r in results)



# --- the gate ------------------------------------------------------------------


def short(text: str) -> str:
    t = norm(text)
    return t if len(t) <= ROW_TEXT else t[: ROW_TEXT - 1] + "…"



@dataclass
class Tools:
    runner: Runner


def run_gate(
    phase: int,
    commit: str,
    root: Path,
    tools: Tools,
    out: Callable[[str], None] = print,
) -> bool:
    """Print the rows for phase N at `commit` (the tree at `root`) and return
    whether the gate passes."""
    roadmap = (root / "docs" / "ROADMAP.md").read_text(encoding="utf-8")
    gates = check_gates.gate_lines(roadmap, phase)
    map_path = root / "tests" / "gates" / f"phase-{phase}.toml"
    ok = bool(gates)
    lines: list[MapLine] | None = None
    try:
        lines = check_gates.load_map(map_path.read_text(encoding="utf-8"),
                                     str(map_path.relative_to(root)))
    except (OSError, check_gates.MapError) as err:
        out(f"MAP   {err}")
        ok = False
    if lines is not None:
        problems = check_gates.validate(phase, lines, gates, check_gates.read_workflow_file(root))
        for p in problems:
            out(f"MAP   {p}")
        if problems:
            ok, lines = False, None
    by_key = {ml.key: ml for ml in lines or []}
    cache = CmdCache(root, phase, tools.runner)
    index = {id(e): i for i, e in enumerate(
        (e for ml in lines or [] for e in ml.entries), start=1)}
    for g in gates:
        if g.tag:
            out(f"TAG   L{g.line}  {short(g.text)}")
            continue
        passed, rows = evaluate_line(g, by_key.get(norm(g.text)), root, cache, index,
                                     lines is not None)
        ok = ok and passed
        out(f"{'PASS' if passed else 'FAIL'}  L{g.line}  {short(g.text)}")
        for r in rows:
            out(r)
    out(f"gate: phase {phase} at {commit}: {'pass' if ok else 'fail'}")
    return ok


def evaluate_line(
    g: GateLine,
    ml: MapLine | None,
    root: Path,
    cache: CmdCache,
    index: dict[int, int],
    mapped: bool,
) -> tuple[bool, list[str]]:
    """(verdict, indented rows) of one non-tag gate line."""
    if not mapped:
        return False, ["      FAIL  the gate map is missing or invalid"]
    if ml is None or not ml.entries:
        return False, ["      FAIL  no entry"]
    results: list[EntryResult] = []
    for e in (x for x in ml.entries if not x.expect_fail):
        if e.kind == "cmd":
            results.append(eval_cmd(e, index[id(e)], cache, root))
        else:
            results.append(EntryResult(e, False, f"{e.kind} entries are not evaluated yet"))
    plain_passed = any(r.ok for r in results)
    for e in (x for x in ml.entries if x.expect_fail):
        if not plain_passed:
            results.append(EntryResult(e, False, "not run: no plain entry of this line passed"))
        elif e.kind == "cmd":
            results.append(eval_cmd(e, index[id(e)], cache, root))
        else:
            results.append(EntryResult(e, False, f"{e.kind} entries are not evaluated yet"))
    rows = [f"      {'ok  ' if r.ok else 'FAIL'}  {r.entry.describe()}"
            + (f"  {r.detail}" if r.detail else "") for r in results]
    return line_verdict(results), rows
