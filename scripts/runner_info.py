#!/usr/bin/env python3
"""A scheduled job's runner record (ROADMAP §10.1, DESIGN §8.6).

GitHub assigns each hosted job's CPU at random, so every scheduled job ends by
writing the host CPU model (the first `model name` of `/proc/cpuinfo`) and
the guest's invariant-TSC bit, read from the `invariant_tsc` marker that
`run_ktest.check_boot_cpu` records in `build/results/`, to `build/runner.json`
(schema 1: os, arch, cpu_model, invtsc), which the job uploads as
`runner-<job>` for the CI-history record's `runner` (C-HISTORY), and, when
`$GITHUB_STEP_SUMMARY` is set, to a table in the job summary.
"""

from __future__ import annotations

import json
import os
import platform
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
RESULTS_DIR = ROOT / "build/results"
OUT = ROOT / "build/runner.json"
SCHEMA = 1
INVTSC_MARKER = "invariant_tsc"
UNKNOWN = "unknown"


def cpu_model(cpuinfo: str) -> str:
    """The first `model name` value of a `/proc/cpuinfo` text, or `unknown`."""
    for line in cpuinfo.splitlines():
        key, sep, value = line.partition(":")
        if sep and key.strip() == "model name" and value.strip():
            return value.strip()
    return UNKNOWN


def guest_invtsc(results_dir: Path) -> bool | None:
    """The `invariant_tsc` marker over every results file: `failed` in any wins,
    then `passed` in any; None when no file records it."""
    passed = False
    for path in sorted(results_dir.glob("*.json")) if results_dir.is_dir() else []:
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        marker = data.get("marker") if isinstance(data, dict) else None
        if not isinstance(marker, dict):
            continue
        if INVTSC_MARKER in (marker.get("failed") or []):
            return False
        if INVTSC_MARKER in (marker.get("passed") or []):
            passed = True
    return True if passed else None


def _invtsc_text(v: object) -> str:
    if v is True:
        return "present"
    if v is False:
        return "absent"
    return "not checked"


def summary(info: dict[str, Any]) -> str:
    """A Markdown table with the CPU model and the invariant-TSC bit side by side."""
    cpu = str(info.get("cpu_model", UNKNOWN)).replace("|", "/")
    return (
        "### Runner\n\n"
        "| OS | Arch | Host CPU model | Guest invariant TSC |\n"
        "|---|---|---|---|\n"
        f"| {info.get('os', UNKNOWN)} | {info.get('arch', UNKNOWN)} | {cpu} "
        f"| {_invtsc_text(info.get('invtsc'))} |\n"
    )


def collect(cpuinfo: str, results_dir: Path) -> dict[str, Any]:
    return {
        "schema": SCHEMA,
        "os": platform.system().lower() or UNKNOWN,
        "arch": platform.machine() or UNKNOWN,
        "cpu_model": cpu_model(cpuinfo),
        "invtsc": guest_invtsc(results_dir),
    }


def _read_cpuinfo() -> str:
    try:
        return Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def main(argv: list[str] | None = None) -> int:
    del argv
    info = collect(_read_cpuinfo(), RESULTS_DIR)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8")
    step_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if step_summary:
        with open(step_summary, "a", encoding="utf-8") as f:
            f.write(summary(info))
    print(f"runner_info: {info['cpu_model']}, invariant tsc {_invtsc_text(info['invtsc'])}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
