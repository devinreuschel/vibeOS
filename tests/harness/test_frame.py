"""Unit tests for kernel-line framing and the serial line checks (DESIGN §2.6, §8.3).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import unittest

import tests.harness.run_ktest as run_ktest
from tests.harness.harness import HarnessError

PAD = run_ktest.SERIAL_PAD
N = run_ktest.SERIAL_WHOLE_N


def whole(i: int) -> str:
    return f"vibeOS: ktest: serial whole {i} of {N} {PAD}"


def noise(cpu: int, n: int, klog: bool = False) -> str:
    kind = "klog " if klog else ""
    return f"vibeOS: ktest: serial noise {kind}cpu{cpu} {n} {PAD}"


def whole_run() -> list[str]:
    out: list[str] = []
    for i in range(N):
        out.append(whole(i))
        if i % 7 == 0:
            out.append(noise(1 + i % 3, i))
            out.append(noise(1 + i % 3, i, klog=True))
    return out


class SerialWholeTests(unittest.TestCase):
    def test_whole_passes(self) -> None:
        lines = whole_run()
        lines.append(f"vibeOS: dmesg: 12ms cpu2 info {noise(2, 3)}")
        lines.append(f"vibeOS: dmesg: 12ms cpu0 info {whole(5)}")
        run_ktest._check_serial_whole(lines)

    def test_split_numbered_line_fails(self) -> None:
        lines = whole_run()
        i = lines.index(whole(500))
        lines[i : i + 1] = [
            "vibeOS: ktest: serial whole 500 of 1000 0123" + noise(1, 9),
            "456789abcdefghijklmnopqrstuvwxyz",
        ]
        with self.assertRaisesRegex(HarnessError, "split line"):
            run_ktest._check_serial_whole(lines)

    def test_missing_number_fails(self) -> None:
        lines = whole_run()
        lines.remove(whole(999))
        with self.assertRaisesRegex(HarnessError, "999 of 1000 numbered lines whole; line 999"):
            run_ktest._check_serial_whole(lines)

    def test_duplicate_number_fails(self) -> None:
        lines = whole_run()
        lines.append(whole(3))
        with self.assertRaisesRegex(HarnessError, "line 3 seen 2 times"):
            run_ktest._check_serial_whole(lines)

    def test_noise_fragment_fails(self) -> None:
        lines = whole_run()
        lines.insert(10, "vibeOS: ktest: serial noise cpu1 4 0123456789")
        with self.assertRaisesRegex(HarnessError, "split line"):
            run_ktest._check_serial_whole(lines)


if __name__ == "__main__":
    unittest.main()
