"""Tests for run_ktest's clocksource check (ROADMAP §10.3, DESIGN §6.4)."""

from __future__ import annotations

import unittest

from tests.harness.frame import FRAME
from tests.harness.harness import HarnessError, QemuConfig
from tests.harness.run_ktest import check_clocksource, expected_clocksource


def _cfg(accel: str, *, hpet: bool = True, extra: tuple[str, ...] = ()) -> QemuConfig:
    return QemuConfig(iso="x.iso", cpu="max", accel=accel, extra=extra, hpet=hpet)


def _lines(*names: str) -> list[str]:
    body = [f"{FRAME}vibeOS: time: clocksource {n}" for n in names]
    return [f"{FRAME}vibeOS: smp: done", *body, f"{FRAME}vibeOS: ktest: end"]


class ExpectedClocksource(unittest.TestCase):
    def test_kvm_wants_tsc(self) -> None:
        self.assertEqual(expected_clocksource(_cfg("kvm")), "tsc")
        self.assertEqual(expected_clocksource(_cfg("kvm", hpet=False)), "tsc")

    def test_aarch64_wants_cntvct(self) -> None:
        cfg = QemuConfig(iso="x.iso", arch="aarch64", accel="tcg")
        self.assertEqual(expected_clocksource(cfg), "cntvct")
        check_clocksource(_lines("cntvct"), cfg)

    def test_tcg_wants_hpet_or_pm_timer(self) -> None:
        self.assertEqual(expected_clocksource(_cfg("tcg")), "hpet")
        self.assertEqual(expected_clocksource(_cfg("tcg", hpet=False)), "acpi_pm")

    def test_machine_hpet_off_in_extra_wants_pm_timer(self) -> None:
        cfg = _cfg("tcg", extra=("-machine", "pc,hpet=off"))
        self.assertEqual(expected_clocksource(cfg), "acpi_pm")

    def test_accel_kvm_in_extra_counts_as_kvm(self) -> None:
        self.assertEqual(expected_clocksource(_cfg("tcg", extra=("-accel", "kvm"))), "tsc")


class CheckClocksource(unittest.TestCase):
    def test_kvm_with_tsc_passes(self) -> None:
        check_clocksource(_lines("tsc"), _cfg("kvm"))

    def test_kvm_with_hpet_fails_naming_tsc(self) -> None:
        with self.assertRaisesRegex(HarnessError, "clocksource tsc.*got.*hpet"):
            check_clocksource(_lines("hpet"), _cfg("kvm"))

    def test_tcg_wants_hpet_then_pm_timer(self) -> None:
        check_clocksource(_lines("hpet"), _cfg("tcg"))
        check_clocksource(_lines("acpi_pm"), _cfg("tcg", hpet=False))
        with self.assertRaisesRegex(HarnessError, "clocksource acpi_pm"):
            check_clocksource(_lines("hpet"), _cfg("tcg", hpet=False))

    def test_missing_line_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "no clocksource line"):
            check_clocksource(_lines(), _cfg("tcg"))

    def test_unframed_line_does_not_count(self) -> None:
        lines = ["vibeOS: time: clocksource hpet", f"{FRAME}vibeOS: ktest: end"]
        with self.assertRaisesRegex(HarnessError, "no clocksource line"):
            check_clocksource(lines, _cfg("tcg"))

    def test_accel_kvm_in_extra_wants_tsc(self) -> None:
        with self.assertRaisesRegex(HarnessError, "clocksource tsc"):
            check_clocksource(_lines("hpet"), _cfg("tcg", extra=("-accel", "kvm")))


if __name__ == "__main__":
    unittest.main()
