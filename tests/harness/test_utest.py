"""Unit tests for the utest verdict (`tests/harness/utest.py`, ROADMAP §10.5),
through `FakeLineSource` (C-LINESOURCE)."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tests.harness import frame, results
from tests.harness.harness import (
    HarnessError,
    Marker,
    QemuConfig,
    run_qemu_and_check,
    run_qemu_console_input,
)
from tests.harness.linesource import FakeLineSource
from tests.harness.skips import SkipRow
from tests.harness.utest import UTEST_PREFIX, UtestVerdict

CFG = QemuConfig(iso="fake.iso")
MARKERS = [
    Marker("vibeOS: serial online", "serial_online"),
    Marker("user: tests ok", "user_tests_ok"),
    Marker("vibeOS: shell ready", "shell_ready"),
]


def K(text: str) -> str:
    """A kernel line: framed (DESIGN §2.6)."""
    return frame.FRAME + text


def U(text: str) -> str:
    """A `/bin/tests` protocol line."""
    return UTEST_PREFIX + text


def boot(utest_lines: list[str], *, after: tuple[str, ...] = ()) -> list[str]:
    """A boot's serial: the kernel's first line, `/bin/tests`' protocol
    lines, its verdict and init's shell."""
    return [
        K("vibeOS: serial online"),
        "user: tests begin",
        *utest_lines,
        *after,
        "user: tests ok",
        "vibeOS: shell ready",
    ]


PASS = [
    U("begin 2"),
    U("run write_count 10000"),
    U("ok write_count"),
    U("run fork_bomb_eagain_at_limit 20000"),
    U("ok fork_bomb_eagain_at_limit"),
    U("end"),
]


class UtestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.res = results.Results("test-utest", out_dir=Path(self.tmp.name))

    def run_boot(
        self, lines: list[str], rows: list[SkipRow] | None = None, *, end: str = "eof"
    ) -> tuple[UtestVerdict, FakeLineSource]:
        verdict = UtestVerdict(CFG, [] if rows is None else rows)
        src = FakeLineSource.from_lines(lines, end=end)
        run_qemu_and_check(CFG, MARKERS, line_source=src, utest=verdict)
        return verdict, src


class TestUtestVerdict(UtestCase):
    def test_utest_pass(self) -> None:
        verdict, src = self.run_boot(boot(PASS))
        self.assertEqual(verdict.passed, ["write_count", "fork_bomb_eagain_at_limit"])
        self.assertEqual(verdict.summary(), "2 run, 0 skipped")
        self.assertTrue(src.quit_sent)

    def test_utest_count_mismatch_fails(self) -> None:
        lines = [U("begin 3"), *PASS[1:]]
        with self.assertRaisesRegex(HarnessError, r"utest count 2 != begin 3"):
            self.run_boot(boot(lines))

    def test_utest_run_without_result_fails(self) -> None:
        lines = [U("begin 2"), U("run a 10000"), U("run b 10000"), U("ok b"), U("end")]
        with self.assertRaisesRegex(HarnessError, r"utest run a has no result"):
            self.run_boot(boot(lines))
        # Open at the last marker.
        lines = [U("begin 1"), U("run a 10000")]
        with self.assertRaisesRegex(HarnessError, r"utest run a has no result before shell_ready"):
            self.run_boot(boot(lines))

    def test_utest_fail_line_fails(self) -> None:
        lines = [U("begin 1"), U("run a 10000"), U("FAIL a: wrong errno"), U("end")]
        with self.assertRaisesRegex(HarnessError, r"utest FAIL: a: wrong errno"):
            self.run_boot(boot(lines))
        self.assertEqual(self.res.sets["utest"]["failed"], {"a"})

    def test_utest_unlisted_skip_fails(self) -> None:
        lines = [U("begin 1"), U("run a 10000"), U("skip a: no init"), U("end")]
        with self.assertRaisesRegex(HarnessError, r"utest skip a not in skips.toml"):
            self.run_boot(boot(lines))
        # A row for another configuration does not match.
        other = [SkipRow("a", "no init", {"smp": (4,)})]
        with self.assertRaisesRegex(HarnessError, r"utest skip a not in skips.toml"):
            self.run_boot(boot(lines), other)
        # Nor does another reason.
        reason = [SkipRow("a", "other")]
        with self.assertRaisesRegex(HarnessError, r"utest skip a not in skips.toml"):
            self.run_boot(boot(lines), reason)

    def test_utest_listed_skip_ok(self) -> None:
        lines = [U("begin 1"), U("run a 10000"), U("skip a: no init"), U("end")]
        verdict, _ = self.run_boot(boot(lines), [SkipRow("a", "no init")])
        self.assertEqual(verdict.summary(), "1 run, 1 skipped")
        self.assertEqual(self.res.sets["utest"]["skipped"], {"a"})

    def test_utest_listed_but_ran_fails(self) -> None:
        lines = [U("begin 1"), U("run a 10000"), U("ok a"), U("end")]
        with self.assertRaisesRegex(HarnessError, r"utest a ran but skips.toml lists it"):
            self.run_boot(boot(lines), [SkipRow("a", "no init")])

    def test_utest_progress_deadline_names_run(self) -> None:
        verdict = UtestVerdict(CFG, [], allowance=60.0, scale=2.0)
        self.assertIsNone(verdict.feed("user: tests begin", now=0.0))
        self.assertEqual(verdict.feed(U("begin 1"), now=1.0), 1.0 + 5.0 * 2)
        self.assertEqual(verdict.feed(U("run slow 20000"), now=2.0), 2.0 + (20.0 + 5.0) * 2)
        self.assertEqual(verdict.hung_message(), "utest hung in slow: no result within 50 s")
        # Through the runner: the run's deadline reaches the source, and a
        # timeout inside the run names it.
        lines = boot([U("begin 1"), U("run slow 20000")])[:4]
        verdict = UtestVerdict(CFG, [])
        src = FakeLineSource.from_lines(lines, end="timeout", exit_code=None)
        with self.assertRaisesRegex(HarnessError, r"utest hung in slow"):
            run_qemu_and_check(CFG, MARKERS, line_source=src, utest=verdict)
        self.assertEqual(len(src.deadlines), 2)

    def test_utest_framed_line_ignored(self) -> None:
        # A kernel line holding the protocol text is not /bin/tests' line,
        # and an unframed one that does not parse fails the boot.
        lines = [*PASS[:3], K(U("FAIL write_count: forged")), *PASS[3:]]
        verdict, _ = self.run_boot(boot(lines))
        self.assertEqual(verdict.summary(), "2 run, 0 skipped")
        # An info line is a case's detail, never a result.
        lines = [*PASS[:2], U("info write_count: read EBADF ok"), *PASS[2:]]
        verdict, _ = self.run_boot(boot(lines))
        self.assertEqual(verdict.summary(), "2 run, 0 skipped")
        with self.assertRaisesRegex(HarnessError, r"utest: malformed line"):
            self.run_boot(boot([*PASS[:1], U("rnu a 1"), *PASS[1:]]))

    def test_utest_missing_begin_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, r"no utest begin before shell_ready"):
            self.run_boot(boot([]))
        with self.assertRaisesRegex(HarnessError, r"utest run a outside begin and end"):
            self.run_boot(boot([U("run a 10000"), U("ok a")]))

    def test_utest_results_recorded(self) -> None:
        lines = [
            U("begin 3"),
            U("run a 10000"),
            U("ok a"),
            U("run b 10000"),
            U("skip b: no init"),
            U("run c 10000"),
            U("ok c"),
            U("end"),
        ]
        self.run_boot(boot(lines), [SkipRow("b", "no init")])
        d = self.res.as_dict()
        self.assertEqual(d["utest"], {"passed": ["a", "c"], "skipped": ["b"], "failed": []})
        self.assertEqual(d["ktest"]["passed"], [])


class TestUtestConsole(UtestCase):
    CONSOLE = [
        "vibeOS: shell ready",
        "vibeos> echo serial-ok",
        "serial-ok",
        "vibeos> false",
        "sh: false: exit 1",
        "vibeos> ps",
        "1 0 run init",
        "vibeos> echo ps2-ok",
        "ps2-ok",
    ]

    def test_utest_console_input_boot(self) -> None:
        lines = [K("vibeOS: serial online"), *PASS, "user: tests ok", *self.CONSOLE]
        verdict = UtestVerdict(CFG, [])
        src = FakeLineSource.from_lines(lines, end="timeout")
        result = run_qemu_console_input(CFG, line_source=src, utest=verdict)
        self.assertEqual(result.matched[-1], "console_input_sh")
        self.assertEqual(verdict.summary(), "2 run, 0 skipped")
        # The verdict finishes at `shell ready`: an open run fails there.
        lines = [K("vibeOS: serial online"), *PASS[:2], *self.CONSOLE]
        src = FakeLineSource.from_lines(lines, end="timeout")
        with self.assertRaisesRegex(HarnessError, r"utest run write_count has no result"):
            run_qemu_console_input(CFG, line_source=src, utest=UtestVerdict(CFG, []))
        # A failing user test fails the console boot too.
        lines = [K("vibeOS: serial online"), *PASS[:2], U("FAIL write_count: x"), *PASS[3:]]
        src = FakeLineSource.from_lines([*lines, *self.CONSOLE], end="timeout")
        with self.assertRaisesRegex(HarnessError, r"utest FAIL: write_count: x"):
            run_qemu_console_input(CFG, line_source=src, utest=UtestVerdict(CFG, []))


if __name__ == "__main__":
    unittest.main()
