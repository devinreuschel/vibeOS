"""Unit tests for tests/harness/results.py (ROADMAP §10.2, DESIGN §8.2)."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tests.harness import frame, results, run_e2e, run_ktest
from tests.harness.harness import (
    ISA_DEBUG_PASS,
    QemuConfig,
    RunResult,
    overlay_env,
)


def K(text: str) -> str:
    """`text` as the kernel prints it on serial: framed (DESIGN §2.6)."""
    return frame.FRAME + text


def _load(path: Path) -> dict[str, object]:
    data = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(data, dict)
    return data


class TestResults(unittest.TestCase):
    def test_schema_1_fields(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            r = results.Results("test-kernel", out_dir=Path(d))
            cfg = QemuConfig(iso="x.iso", smp=4, cpu="max", mem="256M", accel="tcg")
            r.add_boot(["qemu-system-x86_64", "-cdrom", "x.iso"], cfg, 33)
            r.add_boot(["qemu-system-x86_64"], cfg, None)
            path = r.write()
            self.assertEqual(path, Path(d) / "x86_64-test-kernel.json")
            data = _load(path)
        self.assertEqual(
            sorted(data),
            sorted(
                [
                    "schema", "commit", "dirty", "arch", "tier", "qemu",
                    "ktest", "utest", "marker", "retries",
                ]
            ),
        )
        self.assertEqual(data["schema"], 1)
        self.assertEqual(data["arch"], "x86_64")
        self.assertEqual(data["tier"], "test-kernel")
        self.assertIsInstance(data["commit"], str)
        self.assertIsInstance(data["dirty"], bool)
        self.assertEqual(
            data["qemu"],
            [
                {
                    "argv": ["qemu-system-x86_64", "-cdrom", "x.iso"],
                    "smp": 4, "accel": "tcg", "cpu": "max", "mem": "256M", "exit": 33,
                },
                {
                    "argv": ["qemu-system-x86_64"],
                    "smp": 4, "accel": "tcg", "cpu": "max", "mem": "256M", "exit": None,
                },
            ],
        )
        self.assertEqual(data["ktest"], {"passed": [], "skipped": [], "failed": []})
        self.assertEqual(data["utest"], {"passed": [], "skipped": [], "failed": []})
        self.assertEqual(data["marker"], {"passed": [], "failed": []})
        self.assertEqual(data["retries"], [])

    def test_failed_in_any_boot_wins(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            r = results.Results("t", out_dir=Path(d))
            r.record("ktest", "a", "passed")
            r.record("ktest", "a", "failed")
            r.record("ktest", "a", "passed")
            r.record("ktest", "b", "skipped")
            r.record("ktest", "b", "passed")
            r.record("ktest", "b", "passed")
            r.record("marker", "m", "passed")
            data = _load(r.write())
        self.assertEqual(data["ktest"], {"passed": ["b"], "skipped": ["b"], "failed": ["a"]})
        self.assertEqual(data["marker"], {"passed": ["m"], "failed": []})
        with self.assertRaises(ValueError):
            r.record("marker", "m", "skipped")
        with self.assertRaises(ValueError):
            r.record("nope", "m", "passed")
        with self.assertRaises(ValueError):
            r.record("ktest", "m", "ok")

    def test_record_ktest_lines(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            r = results.Results("t", out_dir=Path(d))
            r.record_ktest_lines(
                [
                    "vibeOS: ktest: begin",
                    "vibeOS: ktest: ok alpha",
                    "vibeOS: ktest: skip beta: needs -smp 4",
                    "vibeOS: ktest: FAIL gamma: ready_head should be empty",
                    "junk vibeOS: ktest: ok deltavibeOS: panic: at x",
                    "vibeOS: ktest: ok eps.1vibeOS: ktest: ok zeta",
                    "vibeOS: ktest: end",
                ]
            )
            data = _load(r.write())
        self.assertEqual(
            data["ktest"],
            {
                "passed": ["alpha", "delta", "eps.1", "zeta"],
                "skipped": ["beta"],
                "failed": ["gamma"],
            },
        )

    def test_record_ktest_lines_timed_and_run_lines(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            r = results.Results("t", out_dir=Path(d))
            r.record_ktest_lines(
                [
                    "vibeOS: ktest: begin 3",
                    "vibeOS: ktest: run alpha 10000",
                    "vibeOS: ktest: ok alpha (1234 us)",
                    "vibeOS: ktest: info alpha: spins 3",
                    "vibeOS: ktest: run beta 500",
                    "vibeOS: ktest: FAIL beta: deadline",
                    "vibeOS: ktest: end",
                ]
            )
            data = _load(r.write())
        self.assertEqual(
            data["ktest"], {"passed": ["alpha"], "skipped": [], "failed": ["beta"]}
        )

    def test_missing_marker_and_failure_line(self) -> None:
        self.assertEqual(
            results.missing_marker("timed out after 1.0s; 3/40 markers; missing 'heap_ok'\n--"),
            "heap_ok",
        )
        self.assertEqual(
            results.missing_marker("missing marker 'ktest_end' after 12 lines"), "ktest_end"
        )
        self.assertIsNone(results.missing_marker("panic signature '#GP' in: 'x'"))
        self.assertEqual(
            results.failure_line("timed out after 1.0s; 2 lines\n--- serial tail ---\nx"),
            "timed out after 1.0s; 2 lines",
        )
        self.assertEqual(
            results.failure_line(
                "ktest FAIL: ...",
                ["vibeOS: ktest: ok a", "glue vibeOS: ktest: FAIL b: why", "vibeOS: ktest: FAIL c"],
            ),
            "vibeOS: ktest: FAIL b: why",
        )

    def test_retry_without_summary_env(self) -> None:
        with tempfile.TemporaryDirectory() as d, overlay_env({}, clear=True):
            r = results.Results("t", out_dir=Path(d))
            r.retry("marker boot", "timed out after 1.0s")
            data = _load(r.write())
            self.assertEqual(sorted(p.name for p in Path(d).iterdir()), ["x86_64-t.json"])
        self.assertEqual(
            data["retries"], [{"label": "marker boot", "failure_line": "timed out after 1.0s"}]
        )

    def test_tier_sanitized_in_file_name(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            r = results.Results("a b/c:d.e-f_g", out_dir=Path(d))
            self.assertEqual(r.write().name, "x86_64-a_b_c_d.e-f_g.json")

    def test_e2e_retry_hang_reaches_summary_and_results(self) -> None:
        # Box 1246's proof. No driver retries now (TestNoRetry), so this
        # records the retry an e2e driver would have taken, with its label
        # and the missing marker the timeout names, and checks it reaches
        # both the job summary and the tier's results file.
        msg = (
            "timed out after 1.0s; 3/40 markers; missing 'heap_ok'"
            "\n--- serial tail 1/1 ---\nvibeOS: serial `online`"
        )
        with tempfile.TemporaryDirectory() as d:
            summary = Path(d) / "summary.md"
            out = Path(d) / "results"
            with overlay_env({"GITHUB_STEP_SUMMARY": str(summary)}):
                r = results.Results("test-e2e", out_dir=out)
                results.current().retry("marker boot", results.failure_line(msg))
                run_e2e._record_missing(msg)
            data = _load(r.write())
            text = summary.read_text(encoding="utf-8")
        self.assertEqual(
            data["retries"],
            [
                {
                    "label": "marker boot",
                    "failure_line": "timed out after 1.0s; 3/40 markers; missing 'heap_ok'",
                }
            ],
        )
        self.assertEqual(data["marker"], {"passed": [], "failed": ["heap_ok"]})
        self.assertEqual(
            text,
            "- harness retry (`test-e2e`, marker boot): "
            "`timed out after 1.0s; 3/40 markers; missing 'heap_ok'`\n",
        )

    def test_ktest_boot_records_one_boot_and_no_retry(self) -> None:
        passing = RunResult(
            lines=[
                K("vibeOS: block: vda 8192 sectors"),
                K("vibeOS: block: vdap1 128 sectors"),
                K("vibeOS: block: vdap2 7647 sectors"),
                K("vibeOS: persist: wrote"),
                K("vibeOS: ktest: begin 1"),
                K("vibeOS: ktest: run alpha 10000"),
                K("vibeOS: ktest: ok alpha"),
                K("vibeOS: stack: 16384 used 4096 of 12288 by tid 1 t"),
                K("vibeOS: stack: report 1 sizes 0 lost"),
                # A user program's copy of a verdict is not recorded.
                "?vibeOS: ktest: FAIL forged",
                "vibeOS: ktest: ok forged",
                K("vibeOS: ktest: end"),
            ],
            exit_code=ISA_DEBUG_PASS,
        )
        cfg = QemuConfig(iso="x.iso", smp=2, extra=("-accel", "tcg"))
        with tempfile.TemporaryDirectory() as d:
            summary = Path(d) / "summary.md"
            with overlay_env({"GITHUB_STEP_SUMMARY": str(summary)}):
                r = results.Results("test-kernel", out_dir=Path(d) / "results")
                with mock.patch.object(
                    run_ktest, "run_qemu_until_exit", side_effect=(passing,)
                ):
                    got = run_ktest._ktest_boot(cfg, timeout=1.0, persist_reboot=False)
            data = _load(r.write())
            summary_text = summary.read_text(encoding="utf-8") if summary.exists() else ""
        self.assertIs(got, passing)
        self.assertEqual(data["retries"], [])
        self.assertEqual(data["ktest"], {"passed": ["alpha"], "skipped": [], "failed": []})
        qemu = data["qemu"]
        assert isinstance(qemu, list)
        self.assertEqual(len(qemu), 1)
        self.assertEqual(qemu[0]["exit"], ISA_DEBUG_PASS)
        self.assertEqual(qemu[0]["accel"], "tcg")
        # The stack depth lines, and no retry line (ROADMAP §10.2).
        self.assertNotIn("retry", summary_text)
        self.assertIn("vibeOS: stack: report 1 sizes 0 lost", summary_text)

    def test_run_main_writes_on_failure(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            def failing() -> int:
                r = results.Results("test-e2e", out_dir=Path(d))
                r.record("marker", "heap_ok", "failed")
                return 1

            self.assertEqual(results.run_main(failing), 1)
            data = _load(Path(d) / "x86_64-test-e2e.json")
            self.assertEqual(data["marker"], {"passed": [], "failed": ["heap_ok"]})

            def raising() -> int:
                results.Results("test-kernel", out_dir=Path(d))
                raise RuntimeError("driver bug")

            with self.assertRaises(RuntimeError):
                results.run_main(raising)
            self.assertTrue((Path(d) / "x86_64-test-kernel.json").is_file())

    def test_summary_table_lists_retries(self) -> None:
        with tempfile.TemporaryDirectory() as d, overlay_env({}, clear=True):
            a = results.Results("test-e2e", out_dir=Path(d))
            a.record("marker", "heap_ok", "passed")
            a.retry("marker boot", "timed out after `1.0s`")
            a.write()
            b = results.Results("test-kernel", out_dir=Path(d))
            b.record("ktest", "alpha", "passed")
            b.record("ktest", "beta", "skipped")
            b.write()
            text = results.summary(d)
        self.assertIn("| `test-e2e` | 0 | 0 | 0 | 1 ok, 0 failed | 1 |", text)
        self.assertIn("| `test-kernel` | 1 | 1 | 0 | 0 ok, 0 failed | 0 |", text)
        self.assertIn(
            "- harness retry (`test-e2e`, marker boot): `timed out after '1.0s'`", text
        )


if __name__ == "__main__":
    unittest.main()
