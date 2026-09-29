#!/usr/bin/env python3
"""The phase exit gate (ROADMAP §10.9, the `make gate` and dev-host boxes).

`make gate PHASE=N [RECORD=1] [COMMIT=sha]` runs this script. It prints one
row per exit-gate line of phase N, each followed by its entries' results:

    PASS  L1145  `make check` (fmt, clippy ...
          ok    cmd make check
    FAIL  L1147  `make test` passes on the scheduled macOS CI job ...
          FAIL  cmd test -f .github/workflows/macos.yml  exit 1 (build/gate/phase-10/3.log)
    TAG   L1176  tag `phase-10` and release `v0.10.0`
    BOX   ROADMAP.md:1391  rule A: the top user page is never mappable ...
    gate: phase 10 at <sha>: fail

For N >= 10 it runs every entry of `tests/gates/phase-<N>.toml`
(C-GATEMAP; `scripts/check_gates.py` holds the map's rules, which run
first): a `cmd` entry through `sh -c` at the root, a `job` entry against the
runs of its workflow, and a `record` entry against the dev-host records on
`ci-history`. A line passes only when every entry passes. For N < 10, which
has no map, it runs no entry and needs every gate line but the tag ticked.
Every phase gets the two box rules:

- rule A: an open box under a `### N.M` heading of phase N, outside a
  `### N.M Stretch:` subsection, whose `lands in` notes name no `§M.x` with
  M > N;
- rule B: an open box anywhere in the roadmap whose `lands in` note names a
  `§N.x` of phase N.

A local run gates `HEAD` of a clean work tree (tracked files): `--commit`
must name `HEAD`. `--dry-run` prints the rows and the box problems and runs
nothing. `--record` (RECORD=1) runs only on macOS arm64: it runs each record
entry in a clean worktree of the commit and writes one scrubbed record per
entry through `ci_history.py --record`'s writer.

Exit codes: 0 pass, 1 fail, 2 usage.

Standard library only; `gh` and `git` are its only tools.
"""

from __future__ import annotations

import argparse
import fnmatch
import getpass
import json
import os
import platform
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Protocol

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import check_gates, ci_history, gatelib  # noqa: E402
from scripts.check_gates import Entry, GateLine, MapLine, norm  # noqa: E402

ARCH = "x86_64"
FIRST_MAPPED_PHASE = check_gates.FIRST_MAPPED_PHASE
KTEST_VAR = re.compile(r"(?:^|\s)VIBEOS_KTEST=('[^']*'|\"[^\"]*\"|\S+)")
MAKE_TIER = re.compile(r"\bmake\s+(?:\S+=\S*\s+)*(test-[A-Za-z0-9-]+)")
STRETCH = re.compile(r"^### \d+\.\d+ Stretch:")
SERIAL = re.compile(r'"IOPlatformSerialNumber"\s*=\s*"([^"]*)"')
ROW_TEXT = 100


class GateUsage(Exception):
    """The command line or the checkout cannot be gated (exit 2)."""


# --- the box rules ------------------------------------------------------------


@dataclass(frozen=True)
class BoxProblem:
    line: int
    rule: str  # "A" or "B"
    text: str


def box_problems(roadmap_text: str, phase: int) -> list[BoxProblem]:
    """Rules A and B over every open box of the roadmap, in line order."""
    stretch: set[int] = set()
    under = False
    for n, raw in enumerate(roadmap_text.splitlines(), start=1):
        if raw.startswith("#"):
            under = STRETCH.match(raw) is not None
        elif under:
            stretch.add(n)
    out: list[BoxProblem] = []
    for b in gatelib.parse_boxes(roadmap_text):
        if b.ticked:
            continue
        majors = [m for m, _ in check_gates.lands_in_sections(b.text)]
        in_section = (
            b.phase == phase
            and b.section is not None
            and b.section.split(".")[0] == str(phase)
            and b.line not in stretch
        )
        if in_section and not any(m > phase for m in majors):
            out.append(BoxProblem(b.line, "A", b.text))
        if phase in majors:
            out.append(BoxProblem(b.line, "B", b.text))
    return out


BOX_RULES = {
    "A": "open, and no `lands in §M.x` with M > N",
    "B": "open, and its `lands in` note names a section of phase N",
}


# --- injected tools -----------------------------------------------------------


class Runner(Protocol):
    def __call__(self, cmd: str, cwd: Path, log: Path) -> int: ...


def shell_runner(cmd: str, cwd: Path, log: Path) -> int:
    """`sh -c cmd` in `cwd`, its output in `log`; the exit status."""
    log.parent.mkdir(parents=True, exist_ok=True)
    with open(log, "wb") as f:
        return subprocess.run(["sh", "-c", cmd], cwd=cwd, stdout=f, stderr=subprocess.STDOUT,
                              check=False).returncode


class Gh(Protocol):
    def runs(self, workflow: str, commit: str) -> list[dict[str, Any]]: ...

    def jobs(self, run_id: int) -> list[dict[str, Any]]: ...


class GhCli:
    """Workflow runs through `ci_history.GhApi` (`gh api`)."""

    def __init__(self) -> None:
        self.api = ci_history.GhApi()
        self._repo: str | None = None

    def repo(self) -> str:
        if self._repo is None:
            self._repo = ci_history._repository()
        return self._repo

    def runs(self, workflow: str, commit: str) -> list[dict[str, Any]]:
        path = f"/repos/{self.repo()}/actions/workflows/{workflow}/runs"
        return list(ci_history.paged(self.api, path, "workflow_runs", {"head_sha": commit}))

    def jobs(self, run_id: int) -> list[dict[str, Any]]:
        path = f"/repos/{self.repo()}/actions/runs/{run_id}/jobs"
        return list(ci_history.paged(self.api, path, "jobs", {"filter": "latest"}))


class History(Protocol):
    def runs(self, workflow: str) -> list[dict[str, Any]]: ...

    def dev_host_records(self) -> list[dict[str, Any]]: ...

    def write_record(self, path: Path, forbidden: list[tuple[str, str]]) -> str | None: ...


class BranchHistory:
    """The `ci-history` branch through `ci_history.HistoryRepo`, opened on
    first use, so a gate with no job or record entry never fetches it."""

    def __init__(self) -> None:
        self._repo: ci_history.HistoryRepo | None = None

    def repo(self) -> ci_history.HistoryRepo:
        if self._repo is None:
            self._repo = ci_history.HistoryRepo(ci_history.default_workdir(),
                                                ci_history.default_remote())
            self._repo.open(depth=1)
        return self._repo

    def runs(self, workflow: str) -> list[dict[str, Any]]:
        key = workflow_key(workflow)
        return [] if key is None else self.repo().records(key)

    def dev_host_records(self) -> list[dict[str, Any]]:
        return ci_history.dev_host_records(self.repo())

    def write_record(self, path: Path, forbidden: list[tuple[str, str]]) -> str | None:
        return ci_history.record_main(path, self.repo(), forbidden)


class Host(Protocol):
    def system(self) -> str: ...

    def machine(self) -> str: ...

    def hostnames(self) -> list[str]: ...

    def user(self) -> str: ...

    def home(self) -> str: ...

    def serial(self) -> str: ...

    def mac_model(self) -> str: ...

    def macos(self) -> str: ...

    def qemu(self) -> dict[str, str]: ...


def _out(argv: list[str]) -> str:
    try:
        r = subprocess.run(argv, capture_output=True, text=True, check=False)
    except OSError:
        return ""
    return r.stdout.strip() if r.returncode == 0 else ""


class LocalHost:
    """This machine. `serial` is read for comparison only: never printed."""

    def system(self) -> str:
        return platform.system()

    def machine(self) -> str:
        return platform.machine()

    def hostnames(self) -> list[str]:
        full = socket.gethostname()
        names = [full, full.split(".")[0], _out(["scutil", "--get", "LocalHostName"])]
        return sorted({n for n in names if n})

    def user(self) -> str:
        try:
            return getpass.getuser()
        except (KeyError, OSError):
            return os.environ.get("USER", "")

    def home(self) -> str:
        return str(Path.home())

    def serial(self) -> str:
        m = SERIAL.search(_out(["ioreg", "-rd1", "-c", "IOPlatformExpertDevice"]))
        return m.group(1) if m else ""

    def mac_model(self) -> str:
        return _out(["sysctl", "-n", "hw.model"])

    def macos(self) -> str:
        name = _out(["sw_vers", "-productName"]) or "macOS"
        version = _out(["sw_vers", "-productVersion"])
        return f"{name} {version} ({_out(['sw_vers', '-buildVersion'])})"

    def qemu(self) -> dict[str, str]:
        out: dict[str, str] = {}
        for d in os.environ.get("PATH", "").split(os.pathsep):
            p = Path(d)
            if not p.is_dir():
                continue
            for exe in sorted(p.glob("qemu-system-*")):
                if exe.name not in out and os.access(exe, os.X_OK):
                    first = _out([str(exe), "--version"]).splitlines()
                    out[exe.name] = first[0] if first else ""
        return out


# --- entries ------------------------------------------------------------------


@dataclass(frozen=True)
class EntryResult:
    entry: Entry
    ok: bool
    detail: str = ""


def workflow_key(workflow: str) -> str | None:
    """The `ci_history.WORKFLOWS` key of a workflow file name, or None."""
    for k, spec in ci_history.WORKFLOWS.items():
        if workflow in (k, spec.path, spec.path.rsplit("/", 1)[-1]):
            return k
    return None


def takes_commit(workflow: str) -> bool:
    key = workflow_key(workflow)
    return key is not None and ci_history.WORKFLOWS[key].commit_input


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


def job_name(workflow_text: str, workflow: str, job: str) -> str | None:
    """The display name of job id `job` in a workflow (its `name:`, else the
    id), or None when the workflow has no such job or does not parse."""
    try:
        from scripts import check_workflows

        wf = check_workflows.parse(workflow_text, workflow)
    except Exception:  # noqa: BLE001 - any parse failure means "no such job"
        return None
    jobs = wf.get("jobs")
    node = jobs.get(job) if jobs is not None else None
    if node is None:
        return None
    name = node.get("name")
    return name.value if name is not None and name.value else job


def leg_matches(name: object, want: str) -> bool:
    """A job run is the named job or one of its matrix legs, `<name> (…)`. A
    name built from `${{ … }}` matches on its literal part before the first
    expression."""
    if not isinstance(name, str):
        return False
    literal = want.split("${{", 1)[0].strip()
    if "${{" in want:
        return bool(literal) and name.startswith(literal)
    return name == want or name.startswith(f"{want} (")


def dispatch_hint(workflow: str, commit: str, phase: int) -> list[str]:
    if takes_commit(workflow):
        return [f"gh workflow run {workflow} --ref main -f commit={commit}"]
    return [f"git push origin {commit}:refs/heads/gate/{phase}",
            f"gh workflow run {workflow} --ref gate/{phase}"]


def eval_job(
    e: Entry,
    commit: str,
    phase: int,
    gh: Gh,
    history: History,
    workflow_at: Callable[[str], str | None],
) -> EntryResult:
    """Passes on a green run that proves `commit` (`gatelib.run_proves_commit`)
    whose legs of the named job all concluded `success`. Candidates are the
    API's runs at `commit` and the `ci-history` runs whose commit is `commit`;
    for a workflow that takes a commit as input, a run's history record is
    merged into it. It starts nothing: with no proving run it prints how the
    maintainer starts one."""
    text = workflow_at(e.workflow)
    if text is None:
        return EntryResult(e, False, f"{e.workflow} is not in the tree at {commit[:12]}")
    name = job_name(text, e.workflow, e.job)
    if name is None:
        return EntryResult(e, False, f"job id {e.job} is not in {e.workflow} at {commit[:12]}")
    notes: list[str] = []
    recorded: list[dict[str, Any]] = []
    try:
        recorded = history.runs(e.workflow)
    except ci_history.HistoryError as err:
        notes.append(f"ci-history: {err}")
    by_id = {r.get("run_id"): r for r in recorded}
    candidates: list[tuple[dict[str, Any], list[dict[str, Any]] | None]] = []
    api_runs: list[dict[str, Any]] = []
    try:
        api_runs = gh.runs(e.workflow, commit)
    except (ci_history.HistoryError, OSError) as err:
        notes.append(f"gh: {err}")
    seen: set[object] = set()
    for run in api_runs:
        merged = dict(run)
        rec = by_id.get(run.get("id"))
        if takes_commit(e.workflow) and rec is not None and isinstance(rec.get("commit"), str):
            merged["commit"] = rec["commit"]
        seen.add(run.get("id"))
        candidates.append((merged, None))
    for rec in recorded:
        if rec.get("run_id") not in seen and gatelib.run_commit(rec) == commit:
            candidates.append((rec, rec.get("jobs") if isinstance(rec.get("jobs"), list) else []))
    for run, jobs in candidates:
        if not gatelib.run_proves_commit(run, commit) or run.get("conclusion") != "success":
            continue
        if jobs is None:
            try:
                jobs = gh.jobs(int(run.get("id", 0)))
            except (ci_history.HistoryError, OSError, ValueError) as err:
                notes.append(f"gh: {err}")
                continue
        legs = [j for j in jobs if isinstance(j, dict) and leg_matches(j.get("name"), name)]
        if legs and all(j.get("conclusion") == "success" for j in legs):
            rid = run.get("id", run.get("run_id"))
            return EntryResult(e, True, f"run {rid} ({run.get('event')})")
    hint = "; ".join(dispatch_hint(e.workflow, commit, phase))
    detail = f"no green run of {e.workflow} proves {commit[:12]}; start one: {hint}"
    return EntryResult(e, False, "; ".join([detail, *notes]))


def eval_record(e: Entry, key: str, commit: str, phase: int, history: History) -> EntryResult:
    """Passes only on a `ci-history` dev-host record of this entry at
    `commit`: event `dev-host`, commit, phase, line and command equal, every
    required field present, and result `pass`. Its command never runs."""
    try:
        records = history.dev_host_records()
    except ci_history.HistoryError as err:
        return EntryResult(e, False, f"ci-history: {err}")
    for r in records:
        if (
            r.get("event") == ci_history.DEV_HOST
            and r.get("commit") == commit
            and r.get("phase") == phase
            and isinstance(r.get("line"), str)
            and norm(r["line"]) == key
            and r.get("command") == e.cmd
            and all(f in r for f in ci_history.REQUIRED_RECORD_FIELDS)
            and r.get("result") == "pass"
        ):
            return EntryResult(e, True, f"dev-host record from {r.get('finished')}")
    return EntryResult(e, False, f"ci-history holds no passing dev-host record of it at "
                                 f"{commit[:12]}; run make gate PHASE={phase} RECORD=1 on the "
                                 "dev host")


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
    gh: Gh
    history: History


def git_show(root: Path, commit: str, rel: str) -> str | None:
    r = subprocess.run(["git", "-C", str(root), "show", f"{commit}:{rel}"],
                       capture_output=True, text=True, check=False)
    return r.stdout if r.returncode == 0 else None


def run_gate(
    phase: int,
    commit: str,
    root: Path,
    tools: Tools,
    out: Callable[[str], None] = print,
    *,
    dry_run: bool = False,
    workflow_at: Callable[[str], str | None] | None = None,
) -> bool:
    """Print the rows for phase N at `commit` (the tree at `root`) and return
    whether the gate passes."""
    roadmap = (root / "docs" / "ROADMAP.md").read_text(encoding="utf-8")
    gates = check_gates.gate_lines(roadmap, phase)
    map_path = root / "tests" / "gates" / f"phase-{phase}.toml"
    ok = bool(gates)
    if not gates:
        out(f"gate: phase {phase} has no exit-gate lines in docs/ROADMAP.md")
    lines: list[MapLine] | None = None
    if phase >= FIRST_MAPPED_PHASE:
        if not map_path.is_file():
            out(f"MAP   no tests/gates/phase-{phase}.toml: from Phase {FIRST_MAPPED_PHASE} on "
                "every gate line needs an entry")
            ok = False
        else:
            try:
                lines = check_gates.load_map(map_path.read_text(encoding="utf-8"),
                                             str(map_path.relative_to(root)))
            except check_gates.MapError as err:
                out(f"MAP   {err}")
                ok = False
            if lines is not None:
                problems = check_gates.validate(phase, lines, gates,
                                                check_gates.read_workflow_file(root))
                for p in problems:
                    out(f"MAP   {p}")
                if problems:
                    ok, lines = False, None
    elif map_path.is_file():
        out(f"MAP   tests/gates/phase-{phase}.toml: Phases 0 to {FIRST_MAPPED_PHASE - 1} "
            "get no gate map")
        ok = False
    by_key = {ml.key: ml for ml in lines or []}
    cache = CmdCache(root, phase, tools.runner)
    index = {id(e): i for i, e in enumerate(
        (e for ml in lines or [] for e in ml.entries), start=1)}
    if workflow_at is None:
        def workflow_at(wf: str) -> str | None:
            return git_show(root, commit, f".github/workflows/{wf}")
    for g in gates:
        if g.tag:
            out(f"TAG   L{g.line}  {short(g.text)}")
            continue
        passed, rows = evaluate_line(g, by_key.get(norm(g.text)), phase, commit, root, tools,
                                     cache, index, workflow_at, dry_run, lines is not None)
        ok = ok and passed
        out(f"{'PASS' if passed else 'FAIL'}  L{g.line}  {short(g.text)}")
        for r in rows:
            out(r)
    for bp in box_problems(roadmap, phase):
        ok = False
        out(f"BOX   ROADMAP.md:{bp.line}  rule {bp.rule}: {short(bp.text)}")
    out(f"gate: phase {phase} at {commit}: {'pass' if ok else 'fail'}")
    return ok


def evaluate_line(
    g: GateLine,
    ml: MapLine | None,
    phase: int,
    commit: str,
    root: Path,
    tools: Tools,
    cache: CmdCache,
    index: dict[int, int],
    workflow_at: Callable[[str], str | None],
    dry_run: bool,
    mapped: bool,
) -> tuple[bool, list[str]]:
    """(verdict, indented rows) of one non-tag gate line."""
    if phase < FIRST_MAPPED_PHASE:
        return (True, []) if g.ticked else (False, ["      FAIL  unticked"])
    if not mapped:
        return False, ["      FAIL  the gate map is missing or invalid"]
    if ml is None or not ml.entries:
        return False, ["      FAIL  no entry"]
    if dry_run:
        return True, [f"      -     {e.describe()}  (not run)" for e in ml.entries]
    results: list[EntryResult] = []
    for e in (x for x in ml.entries if not x.expect_fail):
        if e.kind == "cmd":
            results.append(eval_cmd(e, index[id(e)], cache, root))
        elif e.kind == "job":
            results.append(eval_job(e, commit, phase, tools.gh, tools.history, workflow_at))
        else:
            results.append(eval_record(e, ml.key, commit, phase, tools.history))
    plain_passed = any(r.ok for r in results)
    for e in (x for x in ml.entries if x.expect_fail):
        if not plain_passed:
            results.append(EntryResult(e, False, "not run: no plain entry of this line passed"))
        elif e.kind == "record":
            results.append(eval_record(e, ml.key, commit, phase, tools.history))
        else:
            results.append(eval_cmd(e, index[id(e)], cache, root))
    rows = [f"      {'ok  ' if r.ok else 'FAIL'}  {r.entry.describe()}"
            + (f"  {r.detail}" if r.detail else "") for r in results]
    return line_verdict(results), rows


# --- dev-host records ------------------------------------------------------------


def forbidden_values(host: Host) -> list[tuple[str, str]]:
    """(kind, value) pairs no record may hold. The values are compared only,
    never printed: a refusal names the kind."""
    out = [("hostname", h) for h in host.hostnames()]
    out += [("user name", host.user()), ("home directory", host.home()),
            ("serial number", host.serial())]
    return [(k, v) for k, v in out if v]


def scrub(obj: Any, replacements: list[tuple[str, str]]) -> Any:
    """`obj` with each `old` in every string, keys included, replaced by its
    `new`, the longest `old` first."""
    order = sorted((r for r in replacements if r[0]), key=lambda r: -len(r[0]))

    def s(text: str) -> str:
        for old, new in order:
            text = text.replace(old, new)
        return text

    if isinstance(obj, str):
        return s(obj)
    if isinstance(obj, list):
        return [scrub(x, order) for x in obj]
    if isinstance(obj, dict):
        return {s(k) if isinstance(k, str) else k: scrub(v, order) for k, v in obj.items()}
    return obj


def iso(t: float) -> str:
    return datetime.fromtimestamp(t, UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def build_record(
    *,
    host: Host,
    commit: str,
    head_sha: str,
    phase: int,
    key: str,
    command: str,
    ok: bool,
    started: float,
    finished: float,
    results: list[dict[str, Any]],
) -> dict[str, Any]:
    """A dev-host record (ROADMAP §10.9, C-HISTORY): `numbers` holds
    `seconds` and every `numbers` section of the run's results files."""
    numbers: dict[str, Any] = {"seconds": round(finished - started, 1)}
    for r in results:
        extra = r.get("numbers")
        if isinstance(extra, dict):
            numbers.update(extra)
    return {
        "schema": ci_history.SCHEMA,
        "event": ci_history.DEV_HOST,
        "commit": commit,
        "head_sha": head_sha,
        "host": ci_history.DEV_HOST,
        "mac_model": host.mac_model(),
        "macos": host.macos(),
        "qemu": host.qemu(),
        "phase": phase,
        "line": key,
        "command": command,
        "numbers": numbers,
        "result": "pass" if ok else "fail",
        "started": iso(started),
        "finished": iso(finished),
        "results": results,
    }


def worktree_results(checkout: Path) -> list[dict[str, Any]]:
    out = []
    for r in gatelib.load_results(checkout / "build" / "results"):
        r.pop("_path", None)
        out.append(r)
    return out


def record_gate(
    phase: int,
    commit: str,
    root: Path,
    tools: Tools,
    host: Host,
    out: Callable[[str], None] = print,
) -> int:
    """RECORD=1: run each record entry of the map at `commit` in a clean
    worktree of `commit` and write its record. 0 when all passed."""
    if host.system() != "Darwin" or host.machine() != "arm64":
        out(f"gate: RECORD=1 runs only on the Apple Silicon dev host (macOS arm64), "
            f"not {host.system()} {host.machine()}")
        return 2
    rel = f"tests/gates/phase-{phase}.toml"
    text = git_show(root, commit, rel)
    if text is None:
        out(f"gate: {rel} is not in the tree at {commit}")
        return 1
    try:
        lines = check_gates.load_map(text, rel)
    except check_gates.MapError as err:
        out(f"gate: {err}")
        return 1
    todo = [(ml.key, e) for ml in lines for e in ml.entries if e.kind == "record"]
    ids = [ci_history.record_entry_id(phase, k, e.cmd) for k, e in todo]
    if len(set(ids)) != len(ids):
        out("gate: two record entries share one record path (same line and command); "
            "fix the map before recording")
        return 1
    if not todo:
        out(f"gate: phase {phase} has no record entry")
        return 0
    forbidden = forbidden_values(host)
    logs = root / "build" / "gate" / f"phase-{phase}"
    failed = 0
    for i, ((key, e), entry_id) in enumerate(zip(todo, ids, strict=True), start=1):
        with tempfile.TemporaryDirectory(prefix="vibeos-gate-") as tmp_s:
            tmp = Path(tmp_s)
            checkout = tmp / "checkout"
            gatelib.git(root, "worktree", "add", "--detach", "-q", str(checkout), commit)
            try:
                head = gatelib.git(checkout, "rev-parse", "HEAD").strip()
                started = time.time()
                rc = tools.runner(e.cmd, checkout, logs / f"record-{i}.log")
                finished = time.time()
                rec = build_record(host=host, commit=commit, head_sha=head, phase=phase,
                                   key=key, command=e.cmd, ok=rc == 0, started=started,
                                   finished=finished, results=worktree_results(checkout))
            finally:
                gatelib.git(root, "worktree", "remove", "--force", str(checkout), check=False)
                shutil.rmtree(checkout, ignore_errors=True)
            pairs = [(str(checkout.resolve()), "<checkout>"), (str(checkout), "<checkout>"),
                     (str(tmp.resolve()), "<tmp>"), (str(tmp), "<tmp>")]
            rec = scrub(rec, pairs + [(host.home(), "<home>")])
        problems = ci_history.validate_record(rec, forbidden)
        if problems:
            out(f"FAIL  record {e.cmd}: refused: {'; '.join(problems)}")
            failed += 1
            continue
        path = logs / f"record-{entry_id}.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(ci_history.encode(rec))
        try:
            pushed = tools.history.write_record(path, forbidden)
        except ci_history.HistoryError as err:
            out(f"FAIL  record {e.cmd}: {err}")
            failed += 1
            continue
        out(f"{'PASS' if rc == 0 else 'FAIL'}  record {e.cmd}  "
            f"{ci_history.dev_host_record_path(rec)} ({pushed or 'unchanged'})")
        failed += rc != 0
    out(f"gate: phase {phase} records at {commit}: {'pass' if not failed else 'fail'}")
    return 1 if failed else 0


# --- main ------------------------------------------------------------------------


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(
        prog="gate.py",
        description="Phase exit gate (ROADMAP §10.9): run a phase's gate-map entries, print "
        "PASS or FAIL per gate line, and apply the box rules.",
    )
    ap.add_argument("--phase", type=int, required=True, metavar="N")
    ap.add_argument("--record", action="store_true",
                    help="dev host only: run the record entries and write their records")
    ap.add_argument("--commit", default="HEAD", metavar="SHA",
                    help="the gated commit (default HEAD; a local run gates HEAD only)")
    ap.add_argument("--dry-run", action="store_true",
                    help="print the rows and box problems; run nothing")
    ap.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return ap.parse_args(argv)


def resolve(root: Path, rev: str) -> str:
    r = subprocess.run(["git", "-C", str(root), "rev-parse", "--verify", "-q",
                        f"{rev}^{{commit}}"], capture_output=True, text=True, check=False)
    if r.returncode != 0 or not r.stdout.strip():
        raise GateUsage(f"{rev!r} names no commit")
    return r.stdout.strip()


def main(
    argv: list[str] | None = None,
    tools: Tools | None = None,
    host: Host | None = None,
    out: Callable[[str], None] = print,
) -> int:
    try:
        args = parse_args(argv)
    except SystemExit as e:
        return 2 if e.code else 0
    root: Path = args.root
    tools = tools or Tools(shell_runner, GhCli(), BranchHistory())
    try:
        if args.phase < 0:
            raise GateUsage("PHASE is a phase number")
        commit = resolve(root, args.commit)
        if args.record:
            return record_gate(args.phase, commit, root, tools, host or LocalHost(), out)
        if not args.dry_run:
            head = resolve(root, "HEAD")
            if commit != head:
                out(f"gate: a local run gates HEAD ({head}); check out {commit} to gate it")
                return 1
            dirty = gatelib.git(root, "status", "--porcelain", "--untracked-files=no")
            if dirty.strip():
                out("gate: tracked files differ from HEAD; gate a clean checkout:")
                out(dirty.rstrip())
                return 1
    except GateUsage as e:
        out(f"gate: {e}")
        return 2
    except gatelib.GateError as e:
        out(f"gate: {e}")
        return 1
    ok = run_gate(args.phase, commit, root, tools, out, dry_run=args.dry_run)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
