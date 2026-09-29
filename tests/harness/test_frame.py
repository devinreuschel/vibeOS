"""Unit tests for kernel-line framing and the serial line checks (DESIGN §2.6, §8.3).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import unittest

import tests.harness.run_ktest as run_ktest
from tests.harness import frame
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


LIMINE_COLOUR_PANIC = "\x1b[31mPANIC\x1b[37;1m\x1b[0m: stage2 not found"
LIMINE_PLAIN_PANIC = "PANIC: stage2 not found"


def K(text: str) -> str:
    """`text` as the kernel prints it: framed."""
    return frame.FRAME + text


class SplitFrameTests(unittest.TestCase):
    def test_split_frame(self) -> None:
        self.assertEqual(frame.split_frame(K("vibeOS: x")), (True, "vibeOS: x"))
        self.assertEqual(frame.split_frame("vibeOS: x"), (False, "vibeOS: x"))
        self.assertEqual(frame.split_frame(K("")), (True, ""))
        self.assertEqual(frame.split_frame(""), (False, ""))
        # Only the first byte frames: an escaped user 0x1E prints as `?`,
        # and a 0x1E after the start is not a frame.
        self.assertEqual(frame.split_frame("x\x1evibeOS: y"), (False, "x\x1evibeOS: y"))

    def test_kernel_and_user_text(self) -> None:
        self.assertEqual(frame.kernel_text(K("vibeOS: x")), "vibeOS: x")
        self.assertIsNone(frame.kernel_text("vibeOS: x"))
        self.assertEqual(frame.user_text("user: tests ok"), "user: tests ok")
        self.assertIsNone(frame.user_text(K("user: tests ok")))

    def test_text_for(self) -> None:
        self.assertEqual(frame.text_for(frame.KERNEL, K("a")), "a")
        self.assertIsNone(frame.text_for(frame.KERNEL, "a"))
        self.assertEqual(frame.text_for(frame.USER, "a"), "a")
        self.assertIsNone(frame.text_for(frame.USER, K("a")))
        self.assertEqual(frame.text_for(frame.LIMINE, "a"), "a")
        with self.assertRaises(ValueError):
            frame.text_for("firmware", "a")

    def test_kernel_and_user_lines(self) -> None:
        raw = [K("vibeOS: a"), "user: b", K("vibeOS: c"), "?vibeOS: d"]
        self.assertEqual(frame.kernel_lines(raw), ["vibeOS: a", "vibeOS: c"])
        self.assertEqual(frame.user_lines(raw), ["user: b", "?vibeOS: d"])

    def test_source_of(self) -> None:
        for needle in frame.USER_PREFIXES:
            self.assertEqual(frame.source_of(needle), frame.USER, needle)
        self.assertEqual(frame.source_of("init: /bin/tests exited 256"), frame.USER)
        self.assertEqual(frame.source_of("utest_pass foo"), frame.USER)
        self.assertEqual(frame.source_of("vibeOS: serial online"), frame.KERNEL)
        self.assertEqual(frame.source_of("vibeOS: ktest: ok x"), frame.KERNEL)
        self.assertEqual(frame.source_of("user: pid 3 killed SIGSEGV"), frame.KERNEL)

    def test_user_failure(self) -> None:
        self.assertEqual(frame.user_failure("user: tests fail"), "user: tests fail")
        self.assertIsNone(frame.user_failure(K("user: tests fail")))
        self.assertIsNone(frame.user_failure("user: tests ok"))

    def test_limine_panic_regex(self) -> None:
        self.assertIsNotNone(frame.LIMINE_PANIC.search(LIMINE_COLOUR_PANIC))
        self.assertIsNotNone(frame.LIMINE_PANIC.search(LIMINE_PLAIN_PANIC))
        self.assertIsNone(frame.LIMINE_PANIC.search("PANICKED: no"))
        self.assertIsNone(frame.LIMINE_PANIC.search("limine: Loading executable"))

    def test_stream_limine_panic_only_before_first_framed_line(self) -> None:
        s = frame.Stream()
        self.assertFalse(s.kernel_seen)
        loading = "limine: Loading executable"
        self.assertEqual(s.feed(loading), (False, loading))
        self.assertTrue(s.limine_panic(LIMINE_COLOUR_PANIC))
        self.assertTrue(s.limine_panic(LIMINE_PLAIN_PANIC))
        self.assertFalse(s.limine_panic(K(LIMINE_PLAIN_PANIC)))
        self.assertEqual(s.feed(K("vibeOS: serial online")), (True, "vibeOS: serial online"))
        self.assertTrue(s.kernel_seen)
        self.assertFalse(s.limine_panic(LIMINE_COLOUR_PANIC))
        self.assertFalse(s.limine_panic(LIMINE_PLAIN_PANIC))
        self.assertFalse(frame.Stream().limine_panic("user: tests ok"))


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
