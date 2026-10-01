"""Unit tests for declared failure lines (`tests/harness/declared.py`, ROADMAP §10.7).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import unittest
from typing import Any
from unittest import mock

from tests.harness import declared, frame, registry
from tests.harness.harness import (
    ISA_DEBUG_PASS,
    HarnessError,
    KtestDeadlines,
    Marker,
    QemuConfig,
    check_ktest_output,
    declared_in_run,
    run_qemu_and_check,
)
from tests.harness.linesource import FakeLineSource

OVERDUE = "vibeOS: sched: overdue tid <n>"


def K(text: str) -> str:
    """`text` as the kernel prints it: framed."""
    return frame.FRAME + text


BEGIN2 = K("vibeOS: ktest: begin 2")
END = K("vibeOS: ktest: end")
LINE = K("vibeOS: sched: overdue tid 42")


def run(name: str) -> str:
    return K(f"vibeOS: ktest: run {name} 10000")


def ok(name: str) -> str:
    return K(f"vibeOS: ktest: ok {name} (1 us)")


def declaring(test: str = "a") -> Any:
    return mock.patch.dict(declared.DECLARED, {test: (OVERDUE,)}, clear=True)


class DeclaredTests(unittest.TestCase):
    def test_overdue_row_is_a_failure_row(self) -> None:
        rows = [r for r in registry.load_rows() if r.text == OVERDUE]
        self.assertEqual([r.kind for r in rows], ["failure"])

    def test_every_declared_text_is_a_failure_row(self) -> None:
        failures = {r.text for r in registry.load_rows() if r.kind == "failure"}
        for test, texts in declared.DECLARED.items():
            for t in texts:
                with self.subTest(test=test, text=t):
                    self.assertIn(t, failures)

    def test_is_declared_matches_placeholders(self) -> None:
        with declaring():
            self.assertTrue(declared.is_declared("a", "vibeOS: sched: overdue tid 7"))
            self.assertFalse(declared.is_declared("b", "vibeOS: sched: overdue tid 7"))
            self.assertFalse(declared.is_declared(None, "vibeOS: sched: overdue tid 7"))
            self.assertFalse(declared.is_declared("a", "vibeOS: panic: msg: x"))

    def test_missing(self) -> None:
        with declaring():
            self.assertEqual(declared.missing("a", []), [OVERDUE])
            self.assertEqual(declared.missing("a", ["vibeOS: sched: overdue tid 3"]), [])
            self.assertEqual(declared.missing("b", []), [])

    def test_undeclared_failure_line_fails_ktest_run(self) -> None:
        lines = [BEGIN2, run("a"), LINE, ok("a"), run("b"), ok("b"), END]
        with mock.patch.dict(declared.DECLARED, {}, clear=True):
            with self.assertRaisesRegex(HarnessError, "panic signature"):
                check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_declared_line_inside_its_test_passes(self) -> None:
        lines = [BEGIN2, run("a"), LINE, LINE, ok("a"), run("b"), ok("b"), END]
        with declaring():
            check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_declared_line_in_other_test_fails(self) -> None:
        with declaring():
            lines = [BEGIN2, run("a"), LINE, ok("a"), run("b"), LINE, ok("b"), END]
            with self.assertRaisesRegex(HarnessError, "panic signature"):
                check_ktest_output(lines, ISA_DEBUG_PASS)
            # Between two runs is no test's window.
            lines = [BEGIN2, run("a"), LINE, ok("a"), LINE, run("b"), ok("b"), END]
            with self.assertRaisesRegex(HarnessError, "panic signature"):
                check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_declared_line_missing_fails_its_test(self) -> None:
        lines = [BEGIN2, run("a"), ok("a"), run("b"), ok("b"), END]
        with declaring():
            with self.assertRaisesRegex(HarnessError, "ktest: a passed without its declared"):
                check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_failure_line_fails_e2e_run(self) -> None:
        online = "vibeOS: serial online"
        src = FakeLineSource.from_lines([K(online), LINE, K("vibeOS: heap ok")])
        with declaring():
            with self.assertRaisesRegex(HarnessError, "panic signature"):
                run_qemu_and_check(
                    QemuConfig(iso="fake.iso"),
                    [Marker(online, "a"), Marker("vibeOS: heap ok", "b")],
                    line_source=src,
                )

    def test_live_reader_window(self) -> None:
        text = "vibeOS: sched: overdue tid 42"
        with declaring():
            p = KtestDeadlines(10.0)
            p.start(0.0)
            self.assertFalse(declared_in_run(p, text))
            p.on_line(BEGIN2, 1.0)
            p.on_line(run("a"), 2.0)
            self.assertTrue(declared_in_run(p, text))
            self.assertFalse(declared_in_run(None, text))
            p.on_line(ok("a"), 3.0)
            self.assertFalse(declared_in_run(p, text))
            p.on_line(run("b"), 4.0)
            self.assertFalse(declared_in_run(p, text))


if __name__ == "__main__":
    unittest.main()
