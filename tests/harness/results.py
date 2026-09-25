"""Per-tier results files. DESIGN §8.2 / §8.4, ROADMAP §10.2.

Every harness driver writes `build/results/<arch>-<tier>.json` (schema 1),
even when it fails. `<tier>` is `VIBEOS_TIER`, which each Makefile `test-*`
recipe sets to its own target name. Each retry the harness takes lands in the
file's `retries` list and, under GitHub Actions, in the job summary, so a green
run that retried is visible (`check_ticks.py`, ROADMAP §10.9, reads the files).

Several boots in one tier merge into one file: lists are unioned, and a name
that failed in any boot is failed.

Standard library only. `python3 -m tests.harness.results summary <dir>` prints
a markdown table of every results file in `<dir>`.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

from tests.harness.harness import QemuConfig, effective_accel_name

SCHEMA = 1
REPO = Path(__file__).resolve().parents[2]
DEFAULT_OUT_DIR = REPO / "build" / "results"
KINDS = ("ktest", "utest", "marker")
VERDICTS = ("passed", "skipped", "failed")
SUMMARY_LINE_MAX = 300
KTEST_PREFIX = "vibeOS: ktest: "
KTEST_FAIL = "vibeOS: ktest: FAIL"

_KTEST_LINE = re.compile(r"(ok|skip|FAIL) ([A-Za-z0-9_.-]+)")
_MISSING = re.compile(r"missing (?:marker )?['\"]([^'\"]*)['\"]")
_UNSAFE = re.compile(r"[^A-Za-z0-9._-]")

_current: Results | None = None


class Results:
    """One tier's results. Constructing one makes it the current instance."""

    def __init__(self, tier: str, arch: str = "x86_64", out_dir: Path | None = None) -> None:
        global _current
        self.tier = tier
        self.arch = arch
        self.out_dir = DEFAULT_OUT_DIR if out_dir is None else Path(out_dir)
        self.qemu: list[dict[str, Any]] = []
        self.sets: dict[str, dict[str, set[str]]] = {
            kind: {v: set() for v in VERDICTS if not (kind == "marker" and v == "skipped")}
            for kind in KINDS
        }
        self.retries: list[dict[str, str]] = []
        _current = self

    @property
    def path(self) -> Path:
        return self.out_dir / f"{_UNSAFE.sub('_', self.arch)}-{_UNSAFE.sub('_', self.tier)}.json"

    def record(self, kind: str, name: str, verdict: str) -> None:
        if kind not in self.sets:
            raise ValueError(f"unknown results kind {kind!r}")
        lists = self.sets[kind]
        if verdict not in lists:
            raise ValueError(f"unknown verdict {verdict!r} for {kind}")
        if verdict == "failed":
            for v in lists:
                lists[v].discard(name)
            lists["failed"].add(name)
        elif name not in lists["failed"]:
            lists[verdict].add(name)

    def record_ktest_lines(self, lines: Iterable[str]) -> None:
        """Record every `vibeOS: ktest: ok|skip|FAIL <name>` line, glued or not."""
        verdicts = {"ok": "passed", "skip": "skipped", "FAIL": "failed"}
        for line in lines:
            start = line.find(KTEST_PREFIX)
            while start >= 0:
                rest = line[start + len(KTEST_PREFIX) :]
                glued = rest.find("vibeOS:")
                if glued >= 0:
                    rest = rest[:glued]
                m = _KTEST_LINE.match(rest)
                if m is not None:
                    self.record("ktest", m.group(2), verdicts[m.group(1)])
                start = line.find(KTEST_PREFIX, start + len(KTEST_PREFIX))

    def retry(self, label: str, failure_line: str) -> None:
        self.retries.append({"label": label, "failure_line": failure_line})
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if not summary:
            return
        with open(summary, "a", encoding="utf-8") as f:
            f.write(_retry_line(self.tier, label, failure_line) + "\n")

    def add_boot(self, argv: list[str], cfg: QemuConfig, exit_code: int | None) -> None:
        self.qemu.append(
            {
                "argv": list(argv),
                "smp": cfg.smp,
                "accel": effective_accel_name(cfg),
                "cpu": cfg.cpu,
                "mem": cfg.mem,
                "exit": exit_code,
            }
        )

    def as_dict(self) -> dict[str, Any]:
        commit = _git("rev-parse", "HEAD")
        status = _git("status", "--porcelain", "--untracked-files=no")
        return {
            "schema": SCHEMA,
            "commit": commit if commit else "unknown",
            "dirty": bool(status),
            "arch": self.arch,
            "tier": self.tier,
            "qemu": self.qemu,
            **{
                kind: {v: sorted(names) for v, names in lists.items()}
                for kind, lists in self.sets.items()
            },
            "retries": self.retries,
        }

    def write(self) -> Path:
        self.out_dir.mkdir(parents=True, exist_ok=True)
        path = self.path
        path.write_text(json.dumps(self.as_dict(), indent=2) + "\n", encoding="utf-8")
        return path


def _retry_line(tier: str, label: str, failure_line: str) -> str:
    """One job-summary bullet: backticks become `'`, the line is cut to 300 characters."""
    shown = failure_line.replace("`", "'")[:SUMMARY_LINE_MAX]
    return f"- harness retry (`{tier}`, {label}): `{shown}`"


def _git(*args: str) -> str | None:
    try:
        r = subprocess.run(
            ["git", *args], cwd=REPO, capture_output=True, text=True, check=False
        )
    except OSError:
        return None
    if r.returncode != 0:
        return None
    return r.stdout.strip()


def current() -> Results:
    """The driver's instance, or a new unwritten `Results("adhoc")`."""
    if _current is None:
        return Results("adhoc")
    return _current


def run_main(main: Callable[[], int]) -> int:
    """Run a driver's `main` and write the current results, even on failure."""
    try:
        return main()
    finally:
        current().write()


def failure_line(message: str, lines: Iterable[str] = ()) -> str:
    """The first `vibeOS: ktest: FAIL` line, else the message's first line."""
    for line in lines:
        i = line.find(KTEST_FAIL)
        if i >= 0:
            return line[i:]
    return message.split("\n", 1)[0]


def missing_marker(message: str) -> str | None:
    """The marker a harness error names as missing, if any."""
    m = _MISSING.search(message)
    return None if m is None else m.group(1)


def summary(directory: Path | str) -> str:
    """A markdown table per results file, then each retry."""
    files = sorted(Path(directory).glob("*.json"))
    if not files:
        return f"No harness results in `{directory}`.\n"
    out = [
        "| tier | ktest passed | ktest skipped | ktest failed | markers | retries |",
        "|---|---|---|---|---|---|",
    ]
    retries: list[str] = []
    for p in files:
        try:
            d = json.loads(p.read_text(encoding="utf-8"))
        except (OSError, ValueError) as e:
            out.append(f"| `{p.name}` | unreadable: {e} | | | | |")
            continue
        k = d.get("ktest", {})
        mk = d.get("marker", {})
        rs = d.get("retries", [])
        markers = f"{len(mk.get('passed', []))} ok, {len(mk.get('failed', []))} failed"
        out.append(
            f"| `{d.get('tier', p.stem)}` | {len(k.get('passed', []))} "
            f"| {len(k.get('skipped', []))} | {len(k.get('failed', []))} "
            f"| {markers} | {len(rs)} |"
        )
        for r in rs:
            tier = str(d.get("tier", p.stem))
            retries.append(_retry_line(tier, str(r.get("label")), str(r.get("failure_line", ""))))
    text = "\n".join(out) + "\n"
    if retries:
        text += "\nRetries:\n\n" + "\n".join(retries) + "\n"
    return text


def _cli(argv: list[str]) -> int:
    if len(argv) != 2 or argv[0] != "summary":
        print("usage: python3 -m tests.harness.results summary <dir>", file=sys.stderr)
        return 2
    sys.stdout.write(summary(argv[1]))
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli(sys.argv[1:]))
