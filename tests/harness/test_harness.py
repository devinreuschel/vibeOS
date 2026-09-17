"""Unit tests for the e2e harness itself (DESIGN §8.3).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import os
import sys
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import (  # noqa: E402
    DeadlineReader,
    HarnessError,
    Marker,
    QemuConfig,
    check_markers_in_order,
    contains_panic,
    HPET_OFF_MACHINE,
    _qemu_argv,
)


class TestOrderedMarkerCheck(unittest.TestCase):
    def test_all_present_in_order(self) -> None:
        lines = [
            "vibeOS: serial online",
            "vibeOS: limine: rev 3 ok",
            "vibeOS: boot: phase1 done",
        ]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker("vibeOS: limine: rev 3 ok", "b"),
            Marker("vibeOS: boot: phase1 done", "c"),
        ]
        result = check_markers_in_order(lines, markers)
        self.assertEqual(result.matched, ["a", "b", "c"])

    def test_out_of_order_fails(self) -> None:
        lines = [
            "vibeOS: boot: phase1 done",  # too early
            "vibeOS: serial online",
            "vibeOS: limine: rev 3 ok",
        ]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker("vibeOS: limine: rev 3 ok", "b"),
            Marker("vibeOS: boot: phase1 done", "c"),
        ]
        with self.assertRaises(HarnessError):
            check_markers_in_order(lines, markers)

    def test_missing_final_marker_fails(self) -> None:
        lines = ["vibeOS: serial online", "vibeOS: limine: rev 3 ok"]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker("vibeOS: limine: rev 3 ok", "b"),
            Marker("vibeOS: boot: phase1 done", "c"),
        ]
        with self.assertRaises(HarnessError) as cm:
            check_markers_in_order(lines, markers)
        # The error names the missing marker's `name`, not its substring.
        self.assertIn("'c'", str(cm.exception))

    def test_panic_signature_fails_fast(self) -> None:
        lines = [
            "vibeOS: serial online",
            "panicked at src/foo.rs:1:1",
            "vibeOS: boot: phase1 done",
        ]
        markers = [Marker("vibeOS: boot: phase1 done", "c")]
        with self.assertRaises(HarnessError) as cm:
            check_markers_in_order(lines, markers)
        self.assertIn("panicked at", str(cm.exception))

    def test_and_contains_requires_all_fragments(self) -> None:
        # A line that carries only the suffix must NOT satisfy a marker
        # whose shape includes both `vibeOS: pmm:` and the suffix. This
        # is the phase-1 PMM marker's contract per DESIGN §2.6 / §8.3.
        m = Marker(
            "vibeOS: pmm: ",
            "pmm_free_frames",
            and_contains=(" free 4KiB frames",),
        )
        self.assertTrue(m.matches("vibeOS: pmm: 31329 free 4KiB frames"))
        self.assertFalse(m.matches("someone reports 12 free 4KiB frames"))
        self.assertFalse(m.matches("vibeOS: pmm: initializing"))

    def test_and_contains_wrong_shape_fails_ordered_check(self) -> None:
        # The pmm marker must not accept a line that lacks the prefix.
        lines = [
            "vibeOS: serial online",
            "diagnostic: 12 free 4KiB frames on some other subsystem",
            "vibeOS: boot: phase1 done",
        ]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker(
                "vibeOS: pmm: ",
                "pmm",
                and_contains=(" free 4KiB frames",),
            ),
            Marker("vibeOS: boot: phase1 done", "b"),
        ]
        with self.assertRaises(HarnessError) as cm:
            check_markers_in_order(lines, markers)
        self.assertIn("'pmm'", str(cm.exception))

    def test_extra_lines_between_markers_are_fine(self) -> None:
        lines = [
            "chatter",
            "vibeOS: serial online",
            "more chatter",
            "vibeOS: limine: rev 3 ok",
            "even more",
            "vibeOS: boot: phase1 done",
        ]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker("vibeOS: limine: rev 3 ok", "b"),
            Marker("vibeOS: boot: phase1 done", "c"),
        ]
        check_markers_in_order(lines, markers)


class TestPanicSignatureScan(unittest.TestCase):
    def test_matches_exception_mnemonic(self) -> None:
        self.assertTrue(contains_panic("cpu halted on #PF at ..."))
        self.assertTrue(contains_panic("panicked at src/main.rs:12:5"))

    def test_english_prose_is_not_a_false_positive(self) -> None:
        # DESIGN §9.7: matching prose is a footgun. `page fault` must NOT
        # trigger the scanner; only `#PF` does.
        self.assertFalse(contains_panic("shell help: 'demo a page fault'"))
        self.assertFalse(contains_panic("help text about general protection"))

    def test_double_fault_phrase_matches_intentionally(self) -> None:
        # The literal phrase 'double fault' IS listed in PANIC_SIGNATURES.
        # If someone puts it in help text later, they need to rename the
        # help text, not the harness.
        self.assertTrue(contains_panic("we hit a double fault"))


class TestDeadlineReader(unittest.TestCase):
    """Regression coverage for the wedged-pipe bug: a still-open serial pipe
    that stopped producing bytes must not hold the harness past `timeout_s`.
    """

    def test_silent_open_pipe_hits_timeout(self) -> None:
        # Fresh pipe, nothing written. Reader must return ("timeout", "")
        # within a small multiple of the requested deadline.
        r, w = os.pipe()
        try:
            deadline = time.monotonic() + 0.2
            reader = DeadlineReader(r, deadline)
            t0 = time.monotonic()
            kind, _ = reader.next_event()
            elapsed = time.monotonic() - t0
            self.assertEqual(kind, "timeout")
            # Generous slack for CI schedulers; the point is bounded, not zero.
            self.assertLess(elapsed, 1.5)
        finally:
            os.close(r)
            os.close(w)

    def test_reads_available_line_then_times_out(self) -> None:
        r, w = os.pipe()
        try:
            os.write(w, b"vibeOS: serial online\n")
            deadline = time.monotonic() + 0.3
            reader = DeadlineReader(r, deadline)
            kind, payload = reader.next_event()
            self.assertEqual(kind, "line")
            self.assertEqual(payload, "vibeOS: serial online")
            kind2, _ = reader.next_event()
            self.assertEqual(kind2, "timeout")
        finally:
            os.close(r)
            os.close(w)

    def test_partial_line_then_close_flushes_tail(self) -> None:
        r, w = os.pipe()
        try:
            os.write(w, b"partial-without-newline")
            os.close(w)
            w = -1
            reader = DeadlineReader(r, time.monotonic() + 1.0)
            kind, payload = reader.next_event()
            self.assertEqual(kind, "line")
            self.assertEqual(payload, "partial-without-newline")
            kind2, _ = reader.next_event()
            self.assertEqual(kind2, "eof")
        finally:
            os.close(r)
            if w != -1:
                os.close(w)

    def test_crlf_stripped(self) -> None:
        r, w = os.pipe()
        try:
            os.write(w, b"a\r\nb\r\n")
            reader = DeadlineReader(r, time.monotonic() + 0.5)
            self.assertEqual(reader.next_event(), ("line", "a"))
            self.assertEqual(reader.next_event(), ("line", "b"))
        finally:
            os.close(r)
            os.close(w)


class TestKtestProtocol(unittest.TestCase):
    def test_begin_end_pass_status(self) -> None:
        from harness import ISA_DEBUG_PASS, check_ktest_output

        lines = [
            "vibeOS: ktest: begin",
            "vibeOS: ktest: ok map_unmap",
            "vibeOS: ktest: end",
        ]
        check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_fail_line_rejected(self) -> None:
        from harness import ISA_DEBUG_PASS, HarnessError, check_ktest_output

        lines = [
            "vibeOS: ktest: begin",
            "vibeOS: ktest: FAIL nx_enforcement: PF was not instruction-fetch",
            "vibeOS: ktest: end",
        ]
        with self.assertRaises(HarnessError) as cm:
            check_ktest_output(lines, ISA_DEBUG_PASS)
        self.assertIn("FAIL", str(cm.exception))
        self.assertIn("instruction-fetch", str(cm.exception))

    def test_missing_begin_or_end(self) -> None:
        from harness import ISA_DEBUG_PASS, HarnessError, check_ktest_output

        with self.assertRaises(HarnessError):
            check_ktest_output(["vibeOS: ktest: end"], ISA_DEBUG_PASS)
        with self.assertRaises(HarnessError):
            check_ktest_output(["vibeOS: ktest: begin"], ISA_DEBUG_PASS)

    def test_wrong_exit_status(self) -> None:
        from harness import ISA_DEBUG_FAIL, HarnessError, check_ktest_output

        lines = ["vibeOS: ktest: begin", "vibeOS: ktest: end"]
        with self.assertRaises(HarnessError) as cm:
            check_ktest_output(lines, ISA_DEBUG_FAIL)
        self.assertIn("isa-debug-exit", str(cm.exception))


class TestQemuArgv(unittest.TestCase):
    def setUp(self) -> None:
        self._old_accel = os.environ.pop("VIBEOS_QEMU_ACCEL", None)

    def tearDown(self) -> None:
        if self._old_accel is None:
            os.environ.pop("VIBEOS_QEMU_ACCEL", None)
        else:
            os.environ["VIBEOS_QEMU_ACCEL"] = self._old_accel

    def test_hpet_off_uses_machine_property(self) -> None:
        argv = _qemu_argv(QemuConfig(iso="x.iso", hpet=False), "/tmp/mon")
        self.assertEqual(HPET_OFF_MACHINE, ("-machine", "pc,hpet=off"))
        i = argv.index("-machine")
        self.assertEqual(argv[i : i + 2], ["-machine", "pc,hpet=off"])
        self.assertNotIn("-no-hpet", argv)

    def test_hpet_on_has_no_machine_override(self) -> None:
        argv = _qemu_argv(QemuConfig(iso="x.iso"), "/tmp/mon")
        self.assertNotIn("-machine", argv)
        self.assertNotIn("-no-hpet", argv)

    def test_default_accel_is_tcg(self) -> None:
        argv = _qemu_argv(QemuConfig(iso="x.iso"), "/tmp/mon")
        i = argv.index("-accel")
        self.assertEqual(argv[i : i + 2], ["-accel", "tcg"])

    def test_accel_env_override(self) -> None:
        os.environ["VIBEOS_QEMU_ACCEL"] = "kvm"
        argv = _qemu_argv(QemuConfig(iso="x.iso"), "/tmp/mon")
        i = argv.index("-accel")
        self.assertEqual(argv[i : i + 2], ["-accel", "kvm"])
        self.assertEqual(argv.count("-accel"), 1)

    def test_extra_accel_wins(self) -> None:
        argv = _qemu_argv(
            QemuConfig(iso="x.iso", extra=("-accel", "hvf")), "/tmp/mon"
        )
        self.assertEqual(argv.count("-accel"), 1)
        i = argv.index("-accel")
        self.assertEqual(argv[i : i + 2], ["-accel", "hvf"])

    def test_accel_default_omits_flag(self) -> None:
        os.environ["VIBEOS_QEMU_ACCEL"] = "default"
        argv = _qemu_argv(QemuConfig(iso="x.iso"), "/tmp/mon")
        self.assertNotIn("-accel", argv)

    def test_accel_empty_omits_flag(self) -> None:
        os.environ["VIBEOS_QEMU_ACCEL"] = ""
        argv = _qemu_argv(QemuConfig(iso="x.iso"), "/tmp/mon")
        self.assertNotIn("-accel", argv)


if __name__ == "__main__":
    unittest.main()
