"""EL2 boot claims (ROADMAP §11.7).

`virtualization=on` pins the exception-level and timer markers. An EL1
line, a short CPU count, or the other machine's timer fails the boot.
"""

from __future__ import annotations

import unittest
from pathlib import Path

from scripts.check_workflows import parse
from tests.harness.frame import FRAME
from tests.harness.harness import (
    EL2_TIMER_PHYS,
    EL2_TIMER_VIRT,
    EL2_VHE,
    HarnessError,
    Marker,
    QemuConfig,
    boot_contract_markers,
    check_el2_boot,
    run_qemu_and_check,
    virtualization_on,
)
from tests.harness.linesource import FakeLineSource
from tests.harness.run_ktest import check_aarch64_s7

VIRT = "virt,acpi=off,gic-version=3,virtualization=on"
VIRT82 = "virt-8.2,acpi=off,gic-version=3,virtualization=on"
ROOT = Path(__file__).resolve().parents[2]


def K(text: str) -> str:
    return FRAME + text


def marker(machine: str, name: str, smp: int = 1) -> Marker:
    markers = boot_contract_markers(
        arch="aarch64", machine=machine, smp=smp, cpu="max", accel="tcg"
    )
    return next(m for m in markers if m.name == name)


def el_lines(n: int, text: str = f"vibeOS: el: {EL2_VHE}") -> list[str]:
    return [text] * n


class El2Contract(unittest.TestCase):
    def test_virt_pins_vhe_and_hyp_virt(self) -> None:
        el = marker(VIRT, "el")
        timer = marker(VIRT, "time_timer")
        self.assertEqual(el.substring, f"vibeOS: el: {EL2_VHE}")
        self.assertEqual(timer.substring, f"vibeOS: time: timer {EL2_TIMER_VIRT}")
        self.assertTrue(el.matches(K(f"vibeOS: el: {EL2_VHE}")))
        self.assertFalse(el.matches(K("vibeOS: el: 1")))
        self.assertFalse(timer.matches(K(f"vibeOS: time: timer {EL2_TIMER_PHYS}")))
        self.assertFalse(timer.matches(K("vibeOS: time: timer el1 virt")))

    def test_virt_8_2_pins_hyp_phys(self) -> None:
        timer = marker(VIRT82, "time_timer")
        self.assertEqual(timer.substring, f"vibeOS: time: timer {EL2_TIMER_PHYS}")
        self.assertTrue(timer.matches(K(f"vibeOS: time: timer {EL2_TIMER_PHYS}")))
        self.assertFalse(timer.matches(K(f"vibeOS: time: timer {EL2_TIMER_VIRT}")))

    def test_without_virtualization_markers_stay_open(self) -> None:
        self.assertFalse(virtualization_on("virt,acpi=off,gic-version=3"))
        self.assertFalse(virtualization_on("virt,virtualization=off"))
        el = marker("", "el")
        timer = marker("virt,acpi=off,gic-version=3", "time_timer")
        self.assertEqual(el.substring, "vibeOS: el: ")
        self.assertEqual(timer.substring, "vibeOS: time: timer ")
        self.assertTrue(el.matches(K("vibeOS: el: 1")))
        self.assertTrue(timer.matches(K("vibeOS: time: timer el1 virt")))

    def test_scan_rejects_el1_and_the_other_timer(self) -> None:
        pinned = [
            marker(VIRT, "el"),
            marker(VIRT, "time_timer"),
        ]
        el1 = [K("vibeOS: el: 1"), K("vibeOS: time: timer el1 virt")]
        with self.assertRaisesRegex(HarnessError, "missing marker 'el'"):
            run_qemu_and_check(
                QemuConfig(iso="x.iso", arch="aarch64"),
                pinned,
                line_source=FakeLineSource.from_lines(el1),
            )
        same = [
            K(f"vibeOS: el: {EL2_VHE}"),
            K(f"vibeOS: time: timer {EL2_TIMER_PHYS}"),
        ]
        with self.assertRaisesRegex(HarnessError, "missing marker 'time_timer'"):
            run_qemu_and_check(
                QemuConfig(iso="x.iso", arch="aarch64"),
                pinned,
                line_source=FakeLineSource.from_lines(same),
            )
        ok = [
            K(f"vibeOS: el: {EL2_VHE}"),
            K(f"vibeOS: time: timer {EL2_TIMER_VIRT}"),
        ]
        got = run_qemu_and_check(
            QemuConfig(iso="x.iso", arch="aarch64"),
            pinned,
            line_source=FakeLineSource.from_lines(ok),
        )
        self.assertEqual(got.matched, ["el", "time_timer"])


class CheckEl2Boot(unittest.TestCase):
    def test_four_cpus_on_virt(self) -> None:
        lines = el_lines(4) + [f"vibeOS: time: timer {EL2_TIMER_VIRT}"]
        check_el2_boot(lines, VIRT, 4)

    def test_one_el1_cpu_fails(self) -> None:
        lines = el_lines(3) + ["vibeOS: el: 1", f"vibeOS: time: timer {EL2_TIMER_VIRT}"]
        with self.assertRaisesRegex(HarnessError, "want 4"):
            check_el2_boot(lines, VIRT, 4)

    def test_short_count_fails(self) -> None:
        lines = el_lines(1) + [f"vibeOS: time: timer {EL2_TIMER_VIRT}"]
        with self.assertRaisesRegex(HarnessError, "want 4"):
            check_el2_boot(lines, VIRT, 4)

    def test_virt_rejects_physical_timer(self) -> None:
        lines = el_lines(4) + [f"vibeOS: time: timer {EL2_TIMER_PHYS}"]
        with self.assertRaisesRegex(HarnessError, EL2_TIMER_VIRT):
            check_el2_boot(lines, VIRT, 4)

    def test_virt_8_2_rejects_virtual_timer(self) -> None:
        lines = el_lines(4) + [f"vibeOS: time: timer {EL2_TIMER_VIRT}"]
        with self.assertRaisesRegex(HarnessError, EL2_TIMER_PHYS):
            check_el2_boot(lines, VIRT82, 4)

    def test_virt_8_2_accepts_physical_timer(self) -> None:
        lines = el_lines(4) + [f"vibeOS: time: timer {EL2_TIMER_PHYS}"]
        check_el2_boot(lines, VIRT82, 4)

    def test_el1_boot_passes_when_virtualization_is_off(self) -> None:
        lines = ["vibeOS: el: 1", "vibeOS: time: timer el1 virt"]
        check_el2_boot(lines, "virt,acpi=off,gic-version=3", 4)


class CheckAarch64S7El2(unittest.TestCase):
    def _lines(self, timer: str, els: list[str]) -> list[str]:
        return [
            "vibeOS: dt: 42 nodes",
            "vibeOS: vectors ok",
            f"vibeOS: time: timer {timer}",
            "vibeOS: time: cntfrq 24000000/s",
            "vibeOS: gic: v3",
            "vibeOS: time: clocksource cntvct",
            *els,
        ]

    def test_el1_timer_fails_the_ktest_boot(self) -> None:
        cfg = QemuConfig(iso="x.iso", arch="aarch64", machine=VIRT, smp=4)
        with self.assertRaisesRegex(HarnessError, "el2:"):
            check_aarch64_s7(self._lines("el1 virt", el_lines(4, "vibeOS: el: 1")), cfg)

    def test_el2_lines_pass(self) -> None:
        cfg = QemuConfig(iso="x.iso", arch="aarch64", machine=VIRT, smp=4)
        check_aarch64_s7(self._lines(EL2_TIMER_VIRT, el_lines(4)), cfg)


class NightlyEl2Job(unittest.TestCase):
    def test_job_boots_both_machines_and_runs_test_kernel_smp4(self) -> None:
        text = (ROOT / ".github/workflows/nightly.yml").read_text(encoding="utf-8")
        job = parse(text, "nightly.yml").get("jobs")
        assert job is not None
        el2 = job.get("el2")
        assert el2 is not None
        steps = el2.get("steps")
        assert steps is not None
        runs: list[tuple[str, str]] = []
        for step in steps.items:
            run = step.get("run")
            if run is None or run.value is None:
                continue
            env = step.get("env")
            machine = ""
            if env is not None:
                got = env.get("VIBEOS_MACHINE")
                if got is not None and got.value is not None:
                    machine = got.value
            runs.append((run.value, machine))
        e2e = [m for run, m in runs if run == "make test-e2e"]
        self.assertEqual(e2e, [VIRT, VIRT82])
        kernel = [m for run, m in runs if run == "make test-kernel-smp4"]
        self.assertEqual(kernel, [VIRT])


if __name__ == "__main__":
    unittest.main()
