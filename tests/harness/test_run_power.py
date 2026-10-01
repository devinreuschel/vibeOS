"""Tests for run_power.watch_power_boot (ROADMAP §10.5, `make test-e2e-power`)."""

from __future__ import annotations

import itertools
import unittest

from tests.harness import qmp
from tests.harness.frame import FRAME
from tests.harness.harness import KtestDeadlines, QemuConfig
from tests.harness.linesource import FakeLineSource, LineEvent
from tests.harness.qmp import FakeQmp
from tests.harness.run_power import ROWS, PowerBoot, watch_power_boot
from tests.harness.test_qmp import cores_in_tmp, elf_core

ROW, LINE = ROWS[0]
OTHER_ROW, OTHER_LINE = ROWS[1]


def k(text: str) -> str:
    """A framed kernel line."""
    return FRAME + text


def boot_lines(row: str = ROW, line: str = LINE) -> list[str]:
    """A boot that runs `row` and prints `line`."""
    return [
        k("vibeOS: limine: rev 3 ok"),
        k("vibeOS: ktest: begin 1"),
        k(f"vibeOS: ktest: run {row} 10000"),
        k(line),
    ]


def watch(events: list[LineEvent], *, exit_code: int | None = 0, row: str = ROW) -> PowerBoot:
    src = FakeLineSource(events, exit_code=exit_code)
    clock = itertools.count(0.0, 0.01)
    line = dict(ROWS)[row]
    return watch_power_boot(src, row, line, KtestDeadlines(60.0), clock=lambda: next(clock))


def lines(ls: list[str], end: str = "eof") -> list[LineEvent]:
    return [("line", ln) for ln in ls] + [(end, "")]


class WatchPowerBoot(unittest.TestCase):
    def test_pass(self) -> None:
        boot = watch(lines(boot_lines()))
        self.assertIsNone(boot.error)
        self.assertEqual(boot.exit_code, 0)

    def test_restart_row_passes_on_its_line(self) -> None:
        boot = watch(lines(boot_lines(OTHER_ROW, OTHER_LINE)), row=OTHER_ROW)
        self.assertIsNone(boot.error)

    def test_ktest_end_before_exit_fails(self) -> None:
        boot = watch(lines([*boot_lines()[:3], k("vibeOS: ktest: end")]))
        self.assertIsNotNone(boot.error)
        self.assertIn("ktest: end", boot.error or "")

    def test_row_result_fails(self) -> None:
        boot = watch(lines([*boot_lines()[:3], k(f"vibeOS: ktest: FAIL {ROW}: reboot returned")]))
        self.assertIn("ktest FAIL", boot.error or "")

    def test_panic_line_fails(self) -> None:
        boot = watch(lines([*boot_lines()[:3], k("vibeOS: panic: msg: boom")]))
        self.assertIn("panic", boot.error or "")

    def test_no_exit_by_the_deadline_fails(self) -> None:
        boot = watch(lines(boot_lines(), end="timeout"), exit_code=None)
        self.assertIn("no progress", boot.error or "")

    def test_odd_isa_debug_exit_status_fails(self) -> None:
        boot = watch(lines(boot_lines()), exit_code=33)
        self.assertIn("isa-debug-exit", boot.error or "")
        self.assertEqual(boot.exit_code, 33)

    def test_nonzero_exit_fails(self) -> None:
        boot = watch(lines(boot_lines()), exit_code=1)
        self.assertIn("want 0", boot.error or "")

    def test_other_commands_line_fails(self) -> None:
        boot = watch(lines(boot_lines(ROW, OTHER_LINE)))
        self.assertIn("other command", boot.error or "")

    def test_missing_line_fails(self) -> None:
        boot = watch(lines(boot_lines()[:3]))
        self.assertIn("missing", boot.error or "")

    def test_line_before_run_fails(self) -> None:
        boot = watch(lines([k("vibeOS: ktest: begin 1"), k(LINE)]))
        self.assertIn("before the row's run line", boot.error or "")

    def test_unframed_line_is_not_the_marker(self) -> None:
        # A user program cannot forge the kernel's line (DESIGN §2.6).
        boot = watch(lines([*boot_lines()[:3], LINE]))
        self.assertIn("missing", boot.error or "")


class PowerBootQmp(unittest.TestCase):
    """Power boots run under a `qmp.Session` (C-QMP): a timeout takes a
    guest core before QEMU stops (ROADMAP §10.7)."""

    def session(self, fake: FakeQmp) -> qmp.Session:
        s = qmp.Session(QemuConfig(iso="build/vibeos-ktest.iso"), ROW, fake)
        s.start()
        return s

    def test_timeout_takes_a_core(self) -> None:
        with cores_in_tmp("test-e2e-power") as d:
            fake = FakeQmp([], dump=elf_core(2))
            src = FakeLineSource(lines(boot_lines()[:3], end="timeout"), exit_code=None)
            clock = itertools.count(0.0, 0.01)
            boot = watch_power_boot(
                src, ROW, LINE, KtestDeadlines(60.0), clock=lambda: next(clock),
                session=self.session(fake),
            )
            cores = list(d.rglob("core.zst"))
        self.assertIn("no progress", boot.error or "")
        self.assertIn("guest core:", boot.error or "")
        self.assertEqual(len(cores), 1)
        self.assertIn("dump-guest-memory", fake.names())
        self.assertIn("quit", fake.names())

    def test_pass_takes_no_core(self) -> None:
        with cores_in_tmp("test-e2e-power") as d:
            fake = FakeQmp([], dump=elf_core(2))
            src = FakeLineSource([("idle", ""), *lines(boot_lines())])
            clock = itertools.count(0.0, 0.01)
            boot = watch_power_boot(
                src, ROW, LINE, KtestDeadlines(60.0), clock=lambda: next(clock),
                session=self.session(fake),
            )
            cores = list(d.rglob("core.zst"))
        self.assertIsNone(boot.error)
        self.assertEqual(cores, [])
        self.assertNotIn("dump-guest-memory", fake.names())


if __name__ == "__main__":
    unittest.main()
