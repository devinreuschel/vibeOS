"""Tests for run_ktest.check_boot_cpu (ROADMAP §10.1, F078: L1194, L1197)."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tests.harness import results
from tests.harness.harness import HarnessError, QemuConfig
from tests.harness.run_ktest import INVTSC_ABSENT, INVTSC_MARKER, check_boot_cpu


def _cfg(cpu: str, accel: str, extra: tuple[str, ...] = (), hpet: bool = True) -> QemuConfig:
    return QemuConfig(iso="x.iso", cpu=cpu, accel=accel, extra=extra, hpet=hpet)


def _lines(mode: str, *more: str) -> list[str]:
    return ["vibeOS: boot", *more, f"vibeOS: time: lapic_timer ok ({mode})", "vibeOS: ktest: end"]


class CheckBootCpu(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.res = results.Results("test-kernel", out_dir=Path(self._tmp.name))

    def _marker(self, verdict: str) -> set[str]:
        return self.res.sets["marker"][verdict]

    def test_kvm_max_invtsc_passes_on_tsc_deadline(self) -> None:
        check_boot_cpu(_lines("tsc-deadline"), _cfg("max,+invtsc", "kvm"))
        self.assertIn(INVTSC_MARKER, self._marker("passed"))

    def test_kvm_fallback_cpu_passes_on_periodic(self) -> None:
        check_boot_cpu(_lines("periodic"), _cfg("qemu64,-tsc-deadline", "kvm"))

    def test_kvm_fallback_with_invtsc_passes_on_periodic(self) -> None:
        check_boot_cpu(_lines("periodic"), _cfg("qemu64,+invtsc,-tsc-deadline", "kvm"))
        self.assertIn(INVTSC_MARKER, self._marker("passed"))

    def test_tcg_max_passes_on_periodic(self) -> None:
        check_boot_cpu(_lines("periodic"), _cfg("max", "tcg"))

    def test_mismatch_raises_naming_both_modes(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            check_boot_cpu(_lines("periodic"), _cfg("max", "kvm"))
        self.assertIn("tsc-deadline", str(cm.exception))
        self.assertIn("periodic", str(cm.exception))

    def test_missing_line_raises(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            check_boot_cpu(["vibeOS: boot"], _cfg("max", "tcg"))
        self.assertIn("no lapic_timer line", str(cm.exception))

    def test_machine_hpet_off_in_extra_expects_pit(self) -> None:
        cfg = _cfg("max", "tcg", extra=("-machine", "pc,hpet=off"))
        check_boot_cpu(_lines("pit"), cfg)
        with self.assertRaises(HarnessError):
            check_boot_cpu(_lines("periodic"), cfg)

    def test_config_hpet_off_expects_pit(self) -> None:
        check_boot_cpu(_lines("pit"), _cfg("max", "tcg", hpet=False))

    def test_accel_in_extra_wins(self) -> None:
        check_boot_cpu(_lines("tsc-deadline"), _cfg("max", "tcg", extra=("-accel", "kvm")))

    def test_invtsc_absent_raises_and_records_failed(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            check_boot_cpu(_lines("periodic", INVTSC_ABSENT), _cfg("max,+invtsc", "tcg"))
        self.assertIn("invariant tsc absent", str(cm.exception))
        self.assertIn(INVTSC_MARKER, self._marker("failed"))

    def test_invtsc_on_spelling_counts(self) -> None:
        with self.assertRaises(HarnessError):
            check_boot_cpu(_lines("periodic", INVTSC_ABSENT), _cfg("max,invtsc=on", "tcg"))
        self.assertIn(INVTSC_MARKER, self._marker("failed"))

    def test_without_invtsc_the_absent_line_is_ignored(self) -> None:
        check_boot_cpu(_lines("periodic", INVTSC_ABSENT), _cfg("max", "tcg"))
        self.assertNotIn(INVTSC_MARKER, self._marker("passed") | self._marker("failed"))


if __name__ == "__main__":
    unittest.main()
