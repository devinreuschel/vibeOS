"""`make check` fails on a missing ruff, mypy or MSRV toolchain unless the gate
switch VIBEOS_ALLOW_MISSING_TOOLS=1 is set, which skips and names each check
(ROADMAP §10.1, DX1, F147)."""

from __future__ import annotations

import os
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SWITCH = "VIBEOS_ALLOW_MISSING_TOOLS"


def run_make(target: str, *overrides: str, allow: bool = False) -> subprocess.CompletedProcess[str]:
    """Run `make <target> <overrides>` at the repo root. The environment drops
    an enclosing make's jobserver flags (this runs inside `make check`) and
    any inherited switch, and sets the switch only when `allow`."""
    env = {
        k: v
        for k, v in os.environ.items()
        if k not in ("MAKEFLAGS", "MFLAGS", "MAKELEVEL", SWITCH)
    }
    if allow:
        env[SWITCH] = "1"
    return subprocess.run(
        ["make", "-s", "--no-print-directory", "-C", str(ROOT), target, *overrides],
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )


class TestMissingTools(unittest.TestCase):
    def test_missing_ruff_fails(self) -> None:
        proc = run_make("check-python", "RUFF=vibeos-missing-ruff")
        self.assertNotEqual(proc.returncode, 0, proc.stdout)
        self.assertIn("vibeos-missing-ruff not installed", proc.stderr)
        self.assertIn(f"set {SWITCH}=1", proc.stderr)

    def test_missing_mypy_fails(self) -> None:
        proc = run_make("check-python", "RUFF=true", "MYPY=vibeos-missing-mypy")
        self.assertNotEqual(proc.returncode, 0, proc.stdout)
        self.assertIn("vibeos-missing-mypy not installed", proc.stderr)

    def test_allowed_missing_python_tools_skip_and_print(self) -> None:
        proc = run_make(
            "check-python",
            "RUFF=vibeos-missing-ruff",
            "MYPY=vibeos-missing-mypy",
            allow=True,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        skipped = [ln for ln in proc.stdout.splitlines() if ln.startswith("check: skipped ")]
        self.assertEqual(len(skipped), 2, proc.stdout)
        self.assertIn(
            "check: skipped vibeos-missing-ruff check tests scripts: "
            f"vibeos-missing-ruff not installed ({SWITCH}=1)",
            skipped,
        )
        self.assertIn(
            f"check: skipped vibeos-missing-mypy: vibeos-missing-mypy not installed ({SWITCH}=1)",
            skipped,
        )


if __name__ == "__main__":
    unittest.main()
