"""Unit tests for kernel-line framing and the serial line checks (DESIGN §2.6, §8.3).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import dataclasses
import unittest

import tests.harness.run_e2e as run_e2e
import tests.harness.run_ktest as run_ktest
from tests.harness import frame, registry
from tests.harness.harness import (
    ISA_DEBUG_PASS,
    PANIC_DONE,
    PANIC_SIGNATURES,
    HarnessError,
    Marker,
    QemuConfig,
    RunResult,
    check_ktest_output,
    run_qemu_and_check,
    run_qemu_console_input,
)
from tests.harness.linesource import FakeLineSource
from tests.harness.qmp import FakeQmp
from tests.harness.results import Results

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


# One run of a test `x` and its result, for a `begin 1` boot.
RUN_X = K("vibeOS: ktest: run x 10000")
OK_X = K("vibeOS: ktest: ok x (1 us)")


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
        """The marker registry's `source` column decides (ROADMAP §10.2)."""
        for row in registry.load_rows():
            if row.source == frame.USER:
                text = registry.sample(row)
                self.assertEqual(frame.source_of(text), frame.USER, text)
        self.assertEqual(frame.source_of("user: tests ok"), frame.USER)
        self.assertEqual(frame.source_of("vibeOS: shell ready"), frame.USER)
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
        lines.append(f"vibeOS: logrec: 12ms cpu2 info {noise(2, 3)}")
        lines.append(f"vibeOS: dmesg: 12ms cpu0 info {whole(5)}")
        run_ktest._check_serial_whole(lines)

    def test_reads_kernel_lines(self) -> None:
        # `_ktest_boot` hands it `frame.kernel_lines`: a user program's
        # copy of a numbered line is not counted.
        raw = [K(ln) for ln in whole_run()] + [whole(7), "?" + whole(8)]
        run_ktest._check_serial_whole(frame.kernel_lines(raw))

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


FAKE_CFG = QemuConfig(iso="fake.iso")
ONLINE = "vibeOS: serial online"
CONTRACT = "vibeOS: heap ok"


def check(lines: list[str], markers: list[Marker], **kw: object) -> RunResult:
    src = FakeLineSource.from_lines(lines)
    return run_qemu_and_check(FAKE_CFG, markers, line_source=src, **kw)  # type: ignore[arg-type]


class ContractMarkerTests(unittest.TestCase):
    """A kernel marker matches only a framed line."""

    def test_marker_matches(self) -> None:
        m = Marker(CONTRACT, "heap_ok")
        self.assertTrue(m.matches(K(CONTRACT)))
        self.assertFalse(m.matches(CONTRACT))

    def test_framed_marker_passes_run(self) -> None:
        result = check([K(ONLINE), K(CONTRACT)], [Marker(CONTRACT, "heap_ok")])
        self.assertEqual(result.matched, ["heap_ok"])

    def test_unframed_marker_does_not_count(self) -> None:
        with self.assertRaisesRegex(HarnessError, "missing marker 'heap_ok'"):
            check([K(ONLINE), CONTRACT], [Marker(CONTRACT, "heap_ok")])


class UserLineTests(unittest.TestCase):
    """A user program's line matches only unframed (`Marker.source`)."""

    def test_user_lines(self) -> None:
        for text in (
            "vibeOS: shell ready",
            "user: tests begin",
            "user: tests ok",
            "user: dup ok",
            "serial-ok",
        ):
            with self.subTest(text=text):
                m = Marker(text, "u")
                self.assertTrue(m.matches(text))
                self.assertFalse(m.matches(K(text)))
                self.assertEqual(check([K(ONLINE), text], [m]).matched, ["u"])
                with self.assertRaisesRegex(HarnessError, "missing marker 'u'"):
                    check([K(ONLINE), K(text)], [m])

    def test_explicit_source(self) -> None:
        m = Marker("hello", "h", source=frame.USER)
        self.assertTrue(m.matches("hello"))
        self.assertFalse(m.matches(K("hello")))
        k = Marker("user: tests ok", "k", source=frame.KERNEL)
        self.assertTrue(k.matches(K("user: tests ok")))
        self.assertFalse(k.matches("user: tests ok"))

    def test_user_tests_fail_fails_only_unframed(self) -> None:
        with self.assertRaisesRegex(HarnessError, "user failure 'user: tests fail'"):
            check([K(ONLINE), "user: tests fail", K(CONTRACT)], [Marker(CONTRACT, "c")])
        result = check([K(ONLINE), K("user: tests fail"), K(CONTRACT)], [Marker(CONTRACT, "c")])
        self.assertEqual(result.matched, ["c"])


class PanicSignatureTests(unittest.TestCase):
    """Every kernel panic signature fails a run framed and is a user line unframed."""

    def test_each_signature(self) -> None:
        for sig in PANIC_SIGNATURES:
            line = f"{sig} forged here"
            with self.subTest(sig=sig):
                with self.assertRaisesRegex(HarnessError, "panic signature"):
                    check([K(ONLINE), K(line), K(CONTRACT)], [Marker(CONTRACT, "c")])
                result = check([K(ONLINE), line, K(CONTRACT)], [Marker(CONTRACT, "c")])
                self.assertEqual(result.matched, ["c"])
                with self.assertRaisesRegex(HarnessError, "panic signature"):
                    check_ktest_output([K("vibeOS: ktest: begin 1"), K(line)], ISA_DEBUG_PASS)
                check_ktest_output(
                    [K("vibeOS: ktest: begin 1"), RUN_X, line, OK_X, K("vibeOS: ktest: end")],
                    ISA_DEBUG_PASS,
                )

    def test_panic_done_framed_only(self) -> None:
        boot = [K(ONLINE), K("vibeOS: panic:"), K("vibeOS: panic: msg: x")]
        cfg = dataclasses.replace(FAKE_CFG, expect="panic")
        panicked: dict[int, list[dict[str, object]]] = {
            4: [{"event": "GUEST_PANICKED", "data": {"action": "pause"}}]
        }
        src = FakeLineSource.from_lines(boot + [K(PANIC_DONE)])
        run_qemu_and_check(
            cfg, [Marker(ONLINE, "a")], line_source=src, qmp=FakeQmp([], after_line=panicked)
        )
        src = FakeLineSource.from_lines(boot + [PANIC_DONE])
        with self.assertRaisesRegex(HarnessError, "dump ended before"):
            run_qemu_and_check(
                cfg, [Marker(ONLINE, "a")], line_source=src, qmp=FakeQmp([], after_line=panicked)
            )


class KtestVerdictTests(unittest.TestCase):
    """ktest `begin`/`ok`/`FAIL`/`skip`/`end` count only framed."""

    def run_ktest(self, lines: list[str]) -> RunResult:
        return check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_begin_and_end_framed_only(self) -> None:
        self.run_ktest([K("vibeOS: ktest: begin 1"), RUN_X, OK_X, K("vibeOS: ktest: end")])
        with self.assertRaisesRegex(HarnessError, "ktest end without begin"):
            self.run_ktest(["vibeOS: ktest: begin", K("vibeOS: ktest: end")])
        with self.assertRaisesRegex(HarnessError, "ktest_end"):
            self.run_ktest([K("vibeOS: ktest: begin 1"), RUN_X, OK_X, "vibeOS: ktest: end"])

    def test_fail_framed_only(self) -> None:
        with self.assertRaisesRegex(HarnessError, "ktest FAIL"):
            self.run_ktest(
                [
                    K("vibeOS: ktest: begin 1"),
                    RUN_X,
                    K("vibeOS: ktest: FAIL x: y"),
                    K("vibeOS: ktest: end"),
                ]
            )
        self.run_ktest(
            [
                K("vibeOS: ktest: begin 1"),
                RUN_X,
                "vibeOS: ktest: FAIL x: y",
                "?vibeOS: ktest: FAIL forged",
                OK_X,
                K("vibeOS: ktest: end"),
            ]
        )

    def test_ok_and_skip_recorded_framed_only(self) -> None:
        lines = [
            K("vibeOS: ktest: ok a"),
            K("vibeOS: ktest: skip b: no AP"),
            "vibeOS: ktest: ok c",
            "vibeOS: ktest: skip d: no AP",
            "vibeOS: ktest: FAIL e: forged",
        ]
        r = Results("adhoc")
        r.record_ktest_lines(frame.kernel_lines(lines))
        data = r.as_dict()
        self.assertEqual(data["ktest"], {"passed": ["a"], "skipped": ["b"], "failed": []})


CONSOLE = [
    "vibeOS: shell ready",
    "$ echo serial-ok",
    "serial-ok",
    "$ echo ps2-ok",
    "ps2-ok",
]


class ConsoleReplyTests(unittest.TestCase):
    """`shell ready` and the `serial-ok`/`ps2-ok` replies count only unframed."""

    def console(self, lines: list[str]) -> RunResult:
        src = FakeLineSource.from_lines([K(ONLINE), *lines], end="timeout")
        return run_qemu_console_input(FAKE_CFG, line_source=src)

    def test_unframed_passes(self) -> None:
        self.assertEqual(self.console(CONSOLE).matched, ["shell_ready", "serial_echo", "ps2_echo"])

    def test_framed_shell_ready_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "no shell ready"):
            self.console([K(CONSOLE[0]), *CONSOLE[1:]])

    def test_framed_serial_ok_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "serial echo missing"):
            self.console([*CONSOLE[:2], K("serial-ok"), *CONSOLE[3:]])

    def test_framed_ps2_ok_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "PS/2 sendkey echo missing"):
            self.console([*CONSOLE[:4], K("ps2-ok")])

    def test_kernel_panic_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "panic signature"):
            self.console([*CONSOLE[:1], K("vibeOS: panic: x")])

    def test_limine_panic_fails(self) -> None:
        src = FakeLineSource.from_lines([LIMINE_COLOUR_PANIC], end="timeout")
        with self.assertRaisesRegex(HarnessError, "Limine panic"):
            run_qemu_console_input(FAKE_CFG, line_source=src)


class LiminePanicTests(unittest.TestCase):
    """Limine's panic line fails a run only before the first framed line."""

    def test_before_first_framed_line_fails(self) -> None:
        for line in (LIMINE_COLOUR_PANIC, LIMINE_PLAIN_PANIC):
            with self.subTest(line=line):
                src = FakeLineSource.from_lines(["limine: Loading", line, K(ONLINE)])
                with self.assertRaisesRegex(HarnessError, "Limine panic before the kernel"):
                    run_qemu_and_check(FAKE_CFG, [Marker(ONLINE, "a")], line_source=src)
                self.assertTrue(src.killed)
                with self.assertRaisesRegex(HarnessError, "Limine panic"):
                    check_ktest_output([line, K("vibeOS: ktest: begin 1")], ISA_DEBUG_PASS)

    def test_after_first_framed_line_is_a_user_line(self) -> None:
        for line in (LIMINE_COLOUR_PANIC, LIMINE_PLAIN_PANIC):
            with self.subTest(line=line):
                result = check([K(ONLINE), line, K(CONTRACT)], [Marker(CONTRACT, "c")])
                self.assertEqual(result.matched, ["c"])
                check_ktest_output(
                    [K("vibeOS: ktest: begin 1"), RUN_X, line, OK_X, K("vibeOS: ktest: end")],
                    ISA_DEBUG_PASS,
                )


def frame_run() -> list[str]:
    return [
        K("vibeOS: ktest: ok serial_lines_whole"),
        K(run_ktest.SERIAL_FRAME_ESCAPED),
        run_ktest.SERIAL_FRAME_OPEN,
        K(run_ktest.SERIAL_FRAME_AFTER),
        K(run_ktest.SERIAL_FRAME_OK),
    ]


class SerialFrameTests(unittest.TestCase):
    def test_passes(self) -> None:
        run_ktest._check_serial_frame(frame_run())

    def test_escape_missing_fails(self) -> None:
        lines = frame_run()
        lines[1] = K("vibeOS: ktest: serial frame a")
        with self.assertRaisesRegex(HarnessError, "no framed"):
            run_ktest._check_serial_frame(lines)

    def test_unframed_escape_fails(self) -> None:
        lines = frame_run()
        lines[1] = run_ktest.SERIAL_FRAME_ESCAPED
        with self.assertRaisesRegex(HarnessError, "no framed"):
            run_ktest._check_serial_frame(lines)

    def test_open_line_framed_or_glued_fails(self) -> None:
        for bad in (K(run_ktest.SERIAL_FRAME_OPEN), "\x1eserial-frame open",
                    run_ktest.SERIAL_FRAME_OPEN + frame.FRAME + run_ktest.SERIAL_FRAME_AFTER):
            with self.subTest(bad=bad):
                lines = frame_run()
                lines[2] = bad
                with self.assertRaisesRegex(HarnessError, "serial_frame"):
                    run_ktest._check_serial_frame(lines)

    def test_after_before_open_fails(self) -> None:
        lines = frame_run()
        lines[2], lines[3] = lines[3], lines[2]
        with self.assertRaisesRegex(HarnessError, "out of order"):
            run_ktest._check_serial_frame(lines)


def forged_boot() -> list[str]:
    return [
        K(ONLINE),
        "user: tests begin",
        *run_e2e.FORGED_LINES,
        *run_e2e.FORGED_LINES,
        "user: tests ok",
        K(CONTRACT),
    ]


class ForgedLinesTests(unittest.TestCase):
    def test_passes(self) -> None:
        run_e2e._check_forged_lines(forged_boot())

    def test_framed_copy_fails(self) -> None:
        lines = forged_boot() + [K("vibeOS: ktest: FAIL forged")]
        with self.assertRaisesRegex(HarnessError, "printed framed"):
            run_e2e._check_forged_lines(lines)

    def test_missing_escape_fails(self) -> None:
        # The inner 0x1E left unescaped: the line is unframed but not `?#GP?forged`.
        lines = [
            "?#GP\x1eforged" if ln == "?#GP?forged" else ln for ln in forged_boot()
        ]
        with self.assertRaisesRegex(HarnessError, "'\\?#GP\\?forged' seen 0 times"):
            run_e2e._check_forged_lines(lines)

    def test_count_not_two_fails(self) -> None:
        once = [ln for ln in forged_boot() if ln != "?panicked at forged"] + ["?panicked at forged"]
        with self.assertRaisesRegex(HarnessError, "seen 1 times"):
            run_e2e._check_forged_lines(once)
        three = forged_boot() + ["?panicked at forged"]
        with self.assertRaisesRegex(HarnessError, "seen 3 times"):
            run_e2e._check_forged_lines(three)

    def test_recorded_under_its_name(self) -> None:
        r = Results("adhoc")
        run_e2e.forged_user_lines(r, forged_boot())
        self.assertIn("forged_user_lines", r.as_dict()["marker"]["passed"])
        r = Results("adhoc")
        with self.assertRaises(HarnessError):
            run_e2e.forged_user_lines(r, forged_boot()[:-3])
        self.assertIn("forged_user_lines", r.as_dict()["marker"]["failed"])

    def test_forged_lines_do_not_fail_a_run(self) -> None:
        result = check(forged_boot(), [Marker(ONLINE, "a"), Marker(CONTRACT, "c")])
        self.assertEqual(result.matched, ["a", "c"])
        check_ktest_output(
            [K("vibeOS: ktest: begin 1"), RUN_X, *forged_boot(), OK_X, K("vibeOS: ktest: end")],
            ISA_DEBUG_PASS,
        )


if __name__ == "__main__":
    unittest.main()
