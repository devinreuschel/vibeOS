"""Tests for scripts/runner_info.py (ROADMAP §10.1, the KVM leg's runner record)."""

from __future__ import annotations

import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from types import ModuleType
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("runner_info", ROOT / "scripts/runner_info.py")
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


ri = _load()

EPYC = """processor\t: 0
vendor_id\t: AuthenticAMD
cpu family\t: 25
model\t\t: 1
model name\t: AMD EPYC 7763 64-Core Processor
flags\t\t: fpu vme de pse tsc msr constant_tsc nonstop_tsc

processor\t: 1
model name\t: AMD EPYC 7763 64-Core Processor
"""

XEON = """processor\t: 0
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz
processor\t: 1
model name\t: Intel(R) Xeon(R) CPU E5-2673 v4 @ 2.30GHz
"""


def _results(d: Path, name: str, passed: list[str], failed: list[str]) -> None:
    d.mkdir(parents=True, exist_ok=True)
    body = {"schema": 1, "marker": {"passed": passed, "failed": failed}}
    (d / name).write_text(json.dumps(body), encoding="utf-8")


class CpuModel(unittest.TestCase):
    def test_amd_epyc(self) -> None:
        self.assertEqual(ri.cpu_model(EPYC), "AMD EPYC 7763 64-Core Processor")

    def test_intel_xeon_first_wins(self) -> None:
        self.assertEqual(ri.cpu_model(XEON), "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz")

    def test_none(self) -> None:
        self.assertEqual(ri.cpu_model("processor\t: 0\n"), "unknown")


class GuestInvtsc(unittest.TestCase):
    def test_missing_dir_is_none(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            self.assertIsNone(ri.guest_invtsc(Path(d) / "absent"))

    def test_no_marker_is_none(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            _results(Path(d), "x86_64-test-e2e.json", ["boot_ok"], [])
            self.assertIsNone(ri.guest_invtsc(Path(d)))

    def test_passed(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            _results(Path(d), "x86_64-test-kernel.json", ["invariant_tsc"], [])
            self.assertIs(ri.guest_invtsc(Path(d)), True)

    def test_failed_wins_over_passed(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            _results(Path(d), "x86_64-a.json", ["invariant_tsc"], [])
            _results(Path(d), "x86_64-b.json", [], ["invariant_tsc"])
            self.assertIs(ri.guest_invtsc(Path(d)), False)

    def test_unreadable_file_is_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "bad.json").write_text("{", encoding="utf-8")
            _results(Path(d), "x86_64-c.json", ["invariant_tsc"], [])
            self.assertIs(ri.guest_invtsc(Path(d)), True)


class Summary(unittest.TestCase):
    def test_cpu_and_bit_side_by_side(self) -> None:
        info = {"os": "linux", "arch": "x86_64", "cpu_model": "AMD EPYC 7763", "invtsc": True}
        text = ri.summary(info)
        self.assertIn("| Host CPU model | Guest invariant TSC |", text)
        self.assertIn("| AMD EPYC 7763 | present |", text)

    def test_absent_and_unchecked(self) -> None:
        self.assertIn("| absent |", ri.summary({"cpu_model": "x", "invtsc": False}))
        self.assertIn("| not checked |", ri.summary({"cpu_model": "x", "invtsc": None}))


class Main(unittest.TestCase):
    def test_writes_runner_json_and_summary(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            tmp = Path(d)
            _results(tmp / "results", "x86_64-test-kernel.json", [], ["invariant_tsc"])
            out = tmp / "runner.json"
            step = tmp / "summary.md"
            with (
                mock.patch.object(ri, "RESULTS_DIR", tmp / "results"),
                mock.patch.object(ri, "OUT", out),
                mock.patch.object(ri, "_read_cpuinfo", return_value=XEON),
                mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(step)}),
                mock.patch("builtins.print"),
            ):
                self.assertEqual(ri.main([]), 0)
            data = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(
                set(data), {"schema", "os", "arch", "cpu_model", "invtsc"}
            )
            self.assertEqual(data["schema"], 1)
            self.assertEqual(data["cpu_model"], "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz")
            self.assertIs(data["invtsc"], False)
            self.assertIn("| absent |", step.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
