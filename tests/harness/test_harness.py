"""Unit tests for the e2e harness itself (DESIGN §8.3).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import os
import socket
import time
import unittest
from unittest import mock

import tests.harness.run_ktest as run_ktest
from tests.harness.harness import (
    HPET_OFF_MACHINE,
    ISA_DEBUG_FAIL,
    ISA_DEBUG_PASS,
    MCE_DUMP_NEEDLES,
    MCE_MCG_STATUS,
    MCE_UC_STATUS,
    OVMF_BOOT_ARGS,
    SMP4_IPI_WAIT_ACKS_FRAME,
    DeadlineReader,
    HarnessError,
    Marker,
    QemuConfig,
    RunResult,
    _monitor_reply,
    check_markers_in_order,
    check_mce_dump,
    contains_panic,
    drain_panic_tail,
    effective_accel_name,
    mce_monitor_cmd,
    qemu_argv,
    retryable_ktest_failure,
    serial_tail,
    silent_user_syscalls_hang,
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

    def test_serial_tail(self) -> None:
        self.assertEqual(serial_tail([]), " (no serial)")
        self.assertIn("b", serial_tail(["a", "b"], n=1))
        self.assertIn("1/2", serial_tail(["a", "b"], n=1))
        self.assertNotIn("\na\n", serial_tail(["a", "b"], n=1))

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

    def test_pci_count_marker_ignores_per_device_lines(self) -> None:
        m = Marker(
            "vibeOS: pci: ",
            "pci_devices",
            and_contains=(" devices",),
        )
        self.assertTrue(m.matches("vibeOS: pci: 6 devices"))
        self.assertFalse(
            m.matches("vibeOS: pci: 00:00.0 8086:1237 host bridge [440FX]")
        )
        self.assertFalse(m.matches("vibeOS: pci: ecam 0xe0000000 buses 0-255"))
        self.assertFalse(m.matches("vibeOS: pci: skip bar 00:02.0 size 0x10000000000"))

    def test_block_ramdisk_marker_needs_name_and_sectors(self) -> None:
        m = Marker(
            "vibeOS: block: ",
            "block_ramdisk",
            and_contains=(" ram0 ", " sectors"),
        )
        self.assertTrue(m.matches("vibeOS: block: ram0 256 sectors"))
        self.assertFalse(m.matches("vibeOS: block: init"))
        self.assertFalse(m.matches("vibeOS: block: ram0"))
        self.assertFalse(m.matches("ram0 256 sectors"))
        self.assertFalse(m.matches("vibeOS: block: ram0p1 32 sectors"))

    def test_block_partition_marker_parent_pN(self) -> None:
        m = Marker(
            "vibeOS: block: ",
            "block_ram0p1",
            and_contains=(" ram0p1 ", " sectors"),
        )
        self.assertTrue(m.matches("vibeOS: block: ram0p1 32 sectors"))
        self.assertFalse(m.matches("vibeOS: block: ram0 256 sectors"))
        self.assertFalse(m.matches("vibeOS: block: ram0p2 24 sectors"))

    def test_block_vda_marker_needs_name_and_sectors(self) -> None:
        m = Marker(
            "vibeOS: block: ",
            "block_vda",
            and_contains=(" vda ", " sectors"),
        )
        self.assertTrue(m.matches("vibeOS: block: vda 8192 sectors"))
        self.assertFalse(m.matches("vibeOS: block: ram0 256 sectors"))
        self.assertFalse(m.matches("vibeOS: block: vda"))
        self.assertFalse(m.matches("vibeOS: virtio: blk vda"))
        self.assertFalse(m.matches("vibeOS: block: vdap1 128 sectors"))

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
        # The literal phrase 'double fault' is in the harness signature list.
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

    def test_drain_panic_tail_stops_at_halted(self) -> None:
        r, w = os.pipe()
        try:
            os.write(
                w,
                b"msg: ipi: ack timeout waiters=0xd\n"
                b"vibeOS: panic: halted\n"
                b"ignored\n",
            )
            os.close(w)
            w = -1
            result = RunResult(lines=["vibeOS: panic:"])
            reader = DeadlineReader(r, time.monotonic() + 1.0)
            drain_panic_tail(reader, result, window_s=0.5)
            self.assertEqual(
                result.lines,
                [
                    "vibeOS: panic:",
                    "msg: ipi: ack timeout waiters=0xd",
                    "vibeOS: panic: halted",
                ],
            )
        finally:
            os.close(r)
            if w != -1:
                os.close(w)


class TestKtestProtocol(unittest.TestCase):
    def test_begin_end_pass_status(self) -> None:
        from tests.harness.harness import ISA_DEBUG_PASS, check_ktest_output

        lines = [
            "vibeOS: ktest: begin",
            "vibeOS: ktest: ok map_unmap",
            "vibeOS: ktest: end",
        ]
        check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_fail_line_rejected(self) -> None:
        from tests.harness.harness import ISA_DEBUG_PASS, HarnessError, check_ktest_output

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
        from tests.harness.harness import ISA_DEBUG_PASS, HarnessError, check_ktest_output

        with self.assertRaises(HarnessError):
            check_ktest_output(["vibeOS: ktest: end"], ISA_DEBUG_PASS)
        with self.assertRaises(HarnessError):
            check_ktest_output(["vibeOS: ktest: begin"], ISA_DEBUG_PASS)

    def test_wrong_exit_status(self) -> None:
        from tests.harness.harness import ISA_DEBUG_FAIL, HarnessError, check_ktest_output

        lines = ["vibeOS: ktest: begin", "vibeOS: ktest: end"]
        with self.assertRaises(HarnessError) as cm:
            check_ktest_output(lines, ISA_DEBUG_FAIL)
        self.assertIn("isa-debug-exit", str(cm.exception))

    def test_only_smp4_msix_ap_counter_is_retryable(self) -> None:
        msg = "ktest FAIL: vibeOS: ktest: FAIL msix_cpu: ap counter"
        self.assertTrue(retryable_ktest_failure(4, msg))
        self.assertFalse(retryable_ktest_failure(2, msg))
        self.assertFalse(
            retryable_ktest_failure(
                4, "ktest FAIL: vibeOS: ktest: FAIL msix_cpu: bsp counter"
            )
        )

    def test_smp4_ipi_ack_panic_is_retryable_on_first_boot(self) -> None:
        msg = (
            "panic signature 'vibeOS: panic:' in: "
            "'infovibeOS: panic: msg:  ipi: ack timeout waiters=0xdvibeOS: ktes'"
        )
        self.assertTrue(retryable_ktest_failure(4, msg))
        self.assertTrue(
            retryable_ktest_failure(4, msg, persist_reboot=True)
        )
        self.assertFalse(retryable_ktest_failure(2, msg, persist_reboot=True))
        self.assertFalse(
            retryable_ktest_failure(
                4,
                "panic signature 'vibeOS: panic:' in: 'unrelated panic'",
                persist_reboot=True,
            )
        )

    def test_smp4_uart_merged_ktest_panic_is_retryable(self) -> None:
        # CI 35726400930: first-boot smp4, banner glued to ktest ok, no IPI body.
        msg = (
            "panic signature 'vibeOS: panic:' in: "
            "'vibeOS: dmesg: 803ms cpu0 info vibeOS: ktest: ok "
            "tlb_shootdown_remotevibeOS: panic:'"
        )
        self.assertTrue(retryable_ktest_failure(4, msg))
        self.assertTrue(
            retryable_ktest_failure(4, msg, persist_reboot=True)
        )
        self.assertFalse(retryable_ktest_failure(2, msg))
        # serial_tail from an earlier ktest ok must not trip this.
        tailed = (
            "panic signature 'vibeOS: panic:' in: 'vibeOS: panic: at foo.rs'"
            "\n--- serial tail ---\n"
            "vibeOS: ktest: ok map_unmap\n"
            "vibeOS: panic: at foo.rs"
        )
        self.assertFalse(retryable_ktest_failure(4, tailed))

    def test_smp4_wait_acks_frame_is_retryable_when_timeout_line_chopped(self) -> None:
        # CI 35789075345: banner is its own line; last-40 has the idle
        # drain_deferred shootdown backtrace, not `ipi: ack timeout waiters=`.
        msg = (
            "panic signature 'vibeOS: panic:' in: 'vibeOS: panic:'"
            "\n--- serial tail 40/696 ---\n"
            "vibeOS: panic: thread cpu=2 tid=3 idle\n"
            "vibeOS: logrec: 802ms cpu0 info vibeOS: ktest: ok tlb_shootdown_remote\n"
            "vibeOS: logrec: 805msvibeOS: d cpu0mesg:  info vibeOS: ktest: ok shell_dispatch\n"
            "vibeOS: backtrace:\n"
            f"  0xffffffff80049570 vibeos::{SMP4_IPI_WAIT_ACKS_FRAME} "
            "(.llvm.7908952045829238127)+0x100\n"
            "  0xffffffff80049152 vibeos::ipi_init::shootdown_va "
            "(.llvm.7908952045829238127)+0x132\n"
            "  0xffffffff8003dfeb vibeos::kva_init::drain_deferred+0x2ab\n"
            "vibeOS: panic: halted"
        )
        self.assertTrue(retryable_ktest_failure(4, msg))
        self.assertTrue(
            retryable_ktest_failure(4, msg, persist_reboot=True)
        )
        self.assertFalse(retryable_ktest_failure(2, msg))
        chopped = msg.replace(SMP4_IPI_WAIT_ACKS_FRAME, "kva_init::drain_deferred")
        self.assertFalse(retryable_ktest_failure(4, chopped))


class TestSilentUserSyscallsHang(unittest.TestCase):
    def test_dup_ok_timeout_matches(self) -> None:
        msg = (
            "timed out after 90.0s; 101 lines"
            "\n--- serial tail 40/101 ---\n"
            "user: tests begin\n"
            "user: dup ok"
        )
        self.assertTrue(silent_user_syscalls_hang(msg))

    def test_other_timeout_does_not_match(self) -> None:
        msg = "timed out after 90.0s; 40 lines\n--- serial tail 40/40 ---\nvibeOS: ktest: begin"
        self.assertFalse(silent_user_syscalls_hang(msg))
        self.assertFalse(silent_user_syscalls_hang("ktest FAIL: vibeOS: ktest: FAIL x"))


class TestKernelBootRetry(unittest.TestCase):
    @staticmethod
    def _msix_ap_counter_failure() -> RunResult:
        return RunResult(
            lines=[
                "vibeOS: ktest: begin",
                "vibeOS: ktest: FAIL msix_cpu: ap counter",
                "vibeOS: ktest: end",
            ],
            exit_code=ISA_DEBUG_FAIL,
        )

    @staticmethod
    def _per_cpu_ready_head_failure() -> RunResult:
        return RunResult(
            lines=[
                "vibeOS: ktest: begin",
                "vibeOS: ktest: FAIL per_cpu_bsp: ready_head should be empty",
                "vibeOS: ktest: end",
            ],
            exit_code=ISA_DEBUG_FAIL,
        )

    @staticmethod
    def _passing_initial_boot() -> RunResult:
        return RunResult(
            lines=[
                "vibeOS: block: vda 8192 sectors",
                "vibeOS: block: vdap1 128 sectors",
                "vibeOS: block: vdap2 7647 sectors",
                "vibeOS: persist: wrote",
                "vibeOS: ktest: begin",
                "vibeOS: ktest: end",
            ],
            exit_code=ISA_DEBUG_PASS,
        )

    def test_exact_failure_retries_once_then_passes(self) -> None:
        failed = self._msix_ap_counter_failure()
        passed = self._passing_initial_boot()
        cfg = QemuConfig(iso="x.iso", smp=4, extra=("-accel", "tcg"))
        with mock.patch.object(
            run_ktest,
            "run_qemu_until_exit",
            side_effect=(failed, passed),
        ) as run:
            result = run_ktest._ktest_boot(
                cfg,
                timeout=1.0,
                persist_reboot=False,
            )
        self.assertIs(result, passed)
        self.assertEqual(run.call_count, 2)

    def test_exact_failure_stops_after_one_retry(self) -> None:
        failed = self._msix_ap_counter_failure()
        cfg = QemuConfig(iso="x.iso", smp=4, extra=("-accel", "tcg"))
        with mock.patch.object(
            run_ktest,
            "run_qemu_until_exit",
            side_effect=(failed, failed),
        ) as run:
            with self.assertRaises(HarnessError):
                run_ktest._ktest_boot(
                    cfg,
                    timeout=1.0,
                    persist_reboot=False,
                )
        self.assertEqual(run.call_count, 2)

    def test_per_cpu_ready_head_failure_is_not_retried(self) -> None:
        failed = self._per_cpu_ready_head_failure()
        passed = self._passing_initial_boot()
        cfg = QemuConfig(iso="x.iso", smp=2, extra=("-accel", "tcg"))
        with mock.patch.object(
            run_ktest,
            "run_qemu_until_exit",
            side_effect=(failed, passed),
        ) as run:
            with self.assertRaises(HarnessError):
                run_ktest._ktest_boot(
                    cfg,
                    timeout=1.0,
                    persist_reboot=False,
                )
        self.assertEqual(run.call_count, 1)

    @staticmethod
    def _dup_ok_timeout() -> HarnessError:
        return HarnessError(
            "timed out after 90.0s; 101 lines"
            "\n--- serial tail 40/101 ---\n"
            "user: tests begin\n"
            "user: dup ok"
        )

    def test_dup_ok_timeout_gets_a_second_retry(self) -> None:
        hang = self._dup_ok_timeout()
        passed = self._passing_initial_boot()
        cfg = QemuConfig(iso="x.iso", smp=2)
        with mock.patch.object(
            run_ktest,
            "run_qemu_until_exit",
            side_effect=(hang, hang, passed),
        ) as run:
            result = run_ktest._ktest_boot(
                cfg,
                timeout=1.0,
                persist_reboot=False,
            )
        self.assertIs(result, passed)
        self.assertEqual(run.call_count, 3)

    def test_dup_ok_timeout_stops_after_two_retries(self) -> None:
        hang = self._dup_ok_timeout()
        cfg = QemuConfig(iso="x.iso", smp=2)
        with mock.patch.object(
            run_ktest,
            "run_qemu_until_exit",
            side_effect=(hang, hang, hang),
        ) as run:
            with self.assertRaises(HarnessError):
                run_ktest._ktest_boot(
                    cfg,
                    timeout=1.0,
                    persist_reboot=False,
                )
        self.assertEqual(run.call_count, 3)

    def test_other_timeout_still_retries_once(self) -> None:
        other = HarnessError("timed out after 90.0s; 40 lines")
        cfg = QemuConfig(iso="x.iso", smp=2)
        with mock.patch.object(
            run_ktest,
            "run_qemu_until_exit",
            side_effect=(other, other),
        ) as run:
            with self.assertRaises(HarnessError):
                run_ktest._ktest_boot(
                    cfg,
                    timeout=1.0,
                    persist_reboot=False,
                )
        self.assertEqual(run.call_count, 2)

    def test_smp4_uart_merged_panic_retries_once_then_passes(self) -> None:
        err = HarnessError(
            "panic signature 'vibeOS: panic:' in: "
            "'vibeOS: dmesg: 803ms cpu0 info vibeOS: ktest: ok "
            "tlb_shootdown_remotevibeOS: panic:'"
        )
        passed = self._passing_initial_boot()
        cfg = QemuConfig(iso="x.iso", smp=4, extra=("-accel", "tcg"))
        with mock.patch.object(
            run_ktest,
            "run_qemu_until_exit",
            side_effect=(err, passed),
        ) as run:
            result = run_ktest._ktest_boot(
                cfg,
                timeout=1.0,
                persist_reboot=False,
            )
        self.assertIs(result, passed)
        self.assertEqual(run.call_count, 2)

    def test_smp4_wait_acks_frame_retries_once_then_passes(self) -> None:
        err = HarnessError(
            "panic signature 'vibeOS: panic:' in: 'vibeOS: panic:'"
            "\n--- serial tail 40/696 ---\n"
            "vibeOS: panic: thread cpu=2 tid=3 idle\n"
            f"  0xffffffff80049570 vibeos::{SMP4_IPI_WAIT_ACKS_FRAME}+0x100\n"
            "vibeOS: panic: halted"
        )
        passed = self._passing_initial_boot()
        cfg = QemuConfig(iso="x.iso", smp=4, extra=("-accel", "tcg"))
        with mock.patch.object(
            run_ktest,
            "run_qemu_until_exit",
            side_effect=(err, passed),
        ) as run:
            result = run_ktest._ktest_boot(
                cfg,
                timeout=1.0,
                persist_reboot=False,
            )
        self.assertIs(result, passed)
        self.assertEqual(run.call_count, 2)


class TestQemuArgv(unittest.TestCase):
    def test_extra_accel_is_effective(self) -> None:
        tcg = QemuConfig(iso="x.iso", extra=("-accel", "tcg,thread=multi"))
        kvm = QemuConfig(iso="x.iso", extra=("-accel", "kvm"))
        self.assertEqual(effective_accel_name(tcg), "tcg")
        self.assertEqual(effective_accel_name(kvm), "kvm")

    def test_hpet_off_uses_machine_property(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso", hpet=False), "/tmp/mon")
        self.assertEqual(HPET_OFF_MACHINE, ("-machine", "pc,hpet=off"))
        i = argv.index("-machine")
        self.assertEqual(argv[i : i + 2], ["-machine", "pc,hpet=off"])
        self.assertNotIn("-no-hpet", argv)

    def test_hpet_on_has_no_machine_override(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso"), "/tmp/mon")
        self.assertNotIn("-machine", argv)
        self.assertNotIn("-no-hpet", argv)

    def test_default_accel_is_tcg(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso", accel="tcg"), "/tmp/mon")
        i = argv.index("-accel")
        self.assertEqual(argv[i : i + 2], ["-accel", "tcg"])

    def test_accel_kvm_override(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso", accel="kvm"), "/tmp/mon")
        i = argv.index("-accel")
        self.assertEqual(argv[i : i + 2], ["-accel", "kvm"])

    def test_accel_empty_omits_flag(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso", accel=""), "/tmp/mon")
        self.assertNotIn("-accel", argv)

    def test_ovmf_boots_cd_and_disables_pxe(self) -> None:
        argv = qemu_argv(
            QemuConfig(iso="x.iso", bios="/usr/share/ovmf/OVMF.fd"),
            "/tmp/mon",
        )
        self.assertEqual(argv[argv.index("-bios") + 1], "/usr/share/ovmf/OVMF.fd")
        i = argv.index("-boot")
        self.assertEqual(argv[i : i + len(OVMF_BOOT_ARGS)], list(OVMF_BOOT_ARGS))

    def test_seabios_omits_ovmf_boot_args(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso"), "/tmp/mon")
        self.assertNotIn("-bios", argv)
        self.assertNotIn("-boot", argv)
        self.assertNotIn("-fw_cfg", argv)

    def test_no_monitor_omits_flag(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso"), None)
        self.assertNotIn("-monitor", argv)

    def test_boot_order(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso", boot_order="d"), None)
        i = argv.index("-boot")
        self.assertEqual(argv[i : i + 2], ["-boot", "order=d"])

    def test_ovmf_bios_wins_over_boot_order(self) -> None:
        argv = qemu_argv(
            QemuConfig(iso="x.iso", bios="/ovmf.fd", boot_order="d"),
            None,
        )
        self.assertEqual(argv[argv.index("-boot") + 1], "order=d,menu=off")
        self.assertEqual(argv.count("-boot"), 1)


class TestLapicMode(unittest.TestCase):
    def test_tcg_max_is_periodic(self) -> None:
        from tests.harness.harness import expected_lapic_mode

        self.assertEqual(
            expected_lapic_mode(cpu="max", hpet=True, accel="tcg"),
            "periodic",
        )

    def test_hpet_off_is_pit(self) -> None:
        from tests.harness.harness import expected_lapic_mode

        self.assertEqual(
            expected_lapic_mode(cpu="max", hpet=False, accel="tcg"),
            "pit",
        )

    def test_kvm_max_is_tsc_deadline(self) -> None:
        from tests.harness.harness import expected_lapic_mode

        self.assertEqual(
            expected_lapic_mode(cpu="max", hpet=True, accel="kvm"),
            "tsc-deadline",
        )

    def test_cpu_flag_disables_deadline(self) -> None:
        from tests.harness.harness import expected_lapic_mode

        self.assertEqual(
            expected_lapic_mode(
                cpu="qemu64,-tsc-deadline", hpet=True, accel="kvm"
            ),
            "periodic",
        )

    def test_boot_contract_pins_mode(self) -> None:
        from tests.harness.harness import boot_contract_markers

        m = boot_contract_markers(hpet=True, cpu="max", accel="tcg")
        names = [x.name for x in m]
        self.assertIn("lapic_timer_ok", names)
        lapic = next(x for x in m if x.name == "lapic_timer_ok")
        self.assertIn("periodic", lapic.substring)
        pit = boot_contract_markers(hpet=False, cpu="max", accel="tcg")
        lapic_pit = next(x for x in pit if x.name == "lapic_timer_ok")
        self.assertIn("(pit)", lapic_pit.substring)
        names = [x.name for x in boot_contract_markers(smp=2)]
        self.assertIn("smp_ap_online_0", names)
        self.assertIn("sched_cpu1", names)
        self.assertIn("smp_done", names)
        self.assertNotIn("boot_done", names)
        self.assertIn("console_ok", names)
        self.assertIn("pci_devices", names)
        self.assertIn("block_ramdisk", names)
        self.assertIn("block_ram0p1", names)
        self.assertIn("block_ram0p2", names)
        self.assertIn("shell_ready", names)
        smp_i = names.index("smp_done")
        con_i = names.index("console_ok")
        pci_i = names.index("pci_devices")
        blk_i = names.index("block_ramdisk")
        p1_i = names.index("block_ram0p1")
        sh_i = names.index("shell_ready")
        self.assertLess(smp_i, con_i)
        self.assertLess(con_i, pci_i)
        self.assertLess(pci_i, blk_i)
        self.assertLess(blk_i, p1_i)
        self.assertLess(p1_i, sh_i)
        gp_names = [x.name for x in boot_contract_markers(smp=2, gp=True)]
        self.assertIn("console_ok", gp_names)
        self.assertIn("pci_devices", gp_names)
        self.assertIn("block_ramdisk", gp_names)
        self.assertIn("block_ram0p1", gp_names)
        self.assertNotIn("shell_ready", gp_names)
        names1 = [x.name for x in boot_contract_markers(smp=1)]
        self.assertNotIn("smp_ap_online_0", names1)
        self.assertNotIn("sched_cpu1", names1)
        self.assertIn("smp_done", names1)
        names4 = [x.name for x in boot_contract_markers(smp=4)]
        self.assertEqual(sum(1 for n in names4 if n.startswith("smp_ap_online_")), 3)
        self.assertIn("sched_cpu3", names4)


class TestDumpNeedles(unittest.TestCase):
    def test_joint_needle_on_one_line(self) -> None:
        from tests.harness.harness import check_dump_needles, dump_after_panic

        lines = [
            "vibeOS: smp: done",
            "vibeOS: #GP rip=0x1",
            "vibeOS: logrec: 3ms cpu0 info vibeOS: smp: done",
            "vibeOS: backtrace:",
            "vibeOS: panic: halted",
        ]
        dump = dump_after_panic(lines)
        self.assertTrue(dump[0].startswith("vibeOS: #GP"))
        check_dump_needles(
            dump,
            ("#GP", "vibeOS: backtrace:", ("vibeOS: logrec:", "smp: done")),
        )

    def test_english_page_fault_still_ignored(self) -> None:
        from tests.harness.harness import contains_panic

        self.assertFalse(contains_panic("dmesg: page fault help text"))
        self.assertTrue(contains_panic("vibeOS: #PF rip=0x1"))

    def test_missing_joint_needle_fails(self) -> None:
        from tests.harness.harness import HarnessError, check_dump_needles

        with self.assertRaises(HarnessError):
            check_dump_needles(
                ["vibeOS: logrec: hello", "vibeOS: smp: done"],
                (("vibeOS: logrec:", "smp: done"),),
            )


class TestSendkeyChars(unittest.TestCase):
    def test_help_and_echo_chords(self) -> None:
        from tests.harness.harness import sendkey_chars

        self.assertEqual(sendkey_chars("help\n"), "h-e-l-p-ret")
        self.assertEqual(
            sendkey_chars("echo ps2-ok\n"),
            "e-c-h-o-spc-p-s-2-minus-o-k-ret",
        )

    def test_rejects_empty_and_unknown(self) -> None:
        from tests.harness.harness import HarnessError, sendkey_chars

        with self.assertRaises(HarnessError):
            sendkey_chars("")
        with self.assertRaises(HarnessError):
            sendkey_chars("A")


class TestDevicePresets(unittest.TestCase):
    def test_ktest_devices_legacy_off_and_num_queues(self) -> None:
        from tests.harness.harness import ktest_devices

        args = ktest_devices("/tmp/disk.img", 4)
        blob = " ".join(args)
        self.assertIn("disable-legacy=on", blob)
        self.assertIn("num-queues=4", blob)
        self.assertIn("isa-debug-exit", blob)
        self.assertIn("edu", args)
        self.assertIn("e1000e", args)
        self.assertIn("virtio-rng-pci", blob)
        self.assertIn("virtio-blk-pci", blob)
        self.assertIn("discard=unmap", blob)

    def test_ktest_qemu_argv_boot_and_queues(self) -> None:
        from tests.harness.harness import ktest_devices, qemu_argv

        cfg = QemuConfig(
            iso="ktest.iso",
            smp=4,
            extra=ktest_devices("/d", 4),
            boot_order="d",
            accel="tcg",
        )
        argv = qemu_argv(cfg, "/tmp/mon")
        blob = " ".join(argv)
        self.assertIn("-boot order=d", blob)
        self.assertIn("num-queues=4", blob)
        self.assertIn("disable-legacy=on", blob)
        self.assertIn("-monitor unix:/tmp/mon", blob)

    def test_virtio_blk_omit_discard(self) -> None:
        from tests.harness.harness import virtio_blk_args

        with_d = " ".join(virtio_blk_args("/d", 2, discard=True))
        self.assertIn("discard=unmap", with_d)
        without = " ".join(virtio_blk_args("/d", 2, discard=False))
        self.assertNotIn("discard=unmap", without)
        self.assertIn("disable-legacy=on", without)
        self.assertIn("num-queues=2", without)

    def test_kill_delay_window(self) -> None:
        import random

        from tests.harness.harness import CRASH_KILL_MAX_S, kill_delay

        rng = random.Random(0)
        for _ in range(256):
            d = kill_delay(rng)
            self.assertGreaterEqual(d, 0.0)
            self.assertLessEqual(d, CRASH_KILL_MAX_S)


class TestEnvConfig(unittest.TestCase):
    def test_env_flag(self) -> None:
        from tests.harness.harness import env_flag, overlay_env

        with overlay_env({"VIBEOS_GP_TEST": "1"}):
            self.assertTrue(env_flag("VIBEOS_GP_TEST"))
        with overlay_env({"VIBEOS_GP_TEST": "0"}):
            self.assertFalse(env_flag("VIBEOS_GP_TEST"))
        with overlay_env({"VIBEOS_GP_TEST": ""}):
            self.assertFalse(env_flag("VIBEOS_GP_TEST"))
        with overlay_env({}, clear=True):
            self.assertFalse(env_flag("VIBEOS_GP_TEST"))

    def test_env_int(self) -> None:
        from tests.harness.harness import env_int, overlay_env

        with overlay_env({}, clear=True):
            self.assertEqual(env_int("VIBEOS_SMP", 2), 2)
        with overlay_env({"VIBEOS_SMP": "4"}):
            self.assertEqual(env_int("VIBEOS_SMP", 2), 4)
        with overlay_env({"VIBEOS_SMP": ""}):
            self.assertEqual(env_int("VIBEOS_SMP", 2), 2)

    def test_env_config_tier(self) -> None:
        from tests.harness.harness import env_config, overlay_env

        with overlay_env({}, clear=True):
            self.assertEqual(env_config(default_iso="x.iso", default_timeout=1).tier, "adhoc")
        with overlay_env({"VIBEOS_TIER": ""}):
            self.assertEqual(env_config(default_iso="x.iso", default_timeout=1).tier, "adhoc")
        with overlay_env({"VIBEOS_TIER": "test-kernel"}):
            self.assertEqual(
                env_config(default_iso="x.iso", default_timeout=1).tier, "test-kernel"
            )

    def test_env_config_defaults_match_makefile(self) -> None:
        # Keep in sync with Makefile VIBEOS_* ?= (make run). C2.
        import pathlib
        import re

        from tests.harness.harness import env_config, overlay_env

        text = pathlib.Path(__file__).resolve().parents[2].joinpath("Makefile").read_text()
        for key, val in (
            ("VIBEOS_SMP", "2"),
            ("VIBEOS_QEMU_CPU", "max"),
            ("VIBEOS_MEM", "128M"),
            ("VIBEOS_QEMU_ACCEL", "tcg"),
        ):
            self.assertRegex(
                text,
                re.compile(rf"^{re.escape(key)}\s*\?=\s*{re.escape(val)}\s*$", re.M),
            )
        with overlay_env({}, clear=True):
            env = env_config(default_iso="vibeos.iso", default_timeout=60.0)
            self.assertEqual(env.iso, "vibeos.iso")
            self.assertEqual(env.smp, 2)
            self.assertEqual(env.cpu, "max")
            self.assertEqual(env.mem, "128M")
            self.assertIsNone(env.bios)
            self.assertEqual(env.accel, "tcg")
            self.assertEqual(env.timeout, 60.0)
            self.assertEqual(env.extra, ())
            cfg = env.qemu()
            self.assertEqual(cfg.iso, "vibeos.iso")
            self.assertEqual(cfg.smp, 2)
            self.assertEqual(cfg.cpu, "max")
            self.assertEqual(cfg.mem, "128M")
            self.assertEqual(cfg.accel, "tcg")

    def test_env_config_overrides(self) -> None:
        from tests.harness.harness import env_config, overlay_env

        with overlay_env(
            {
                "VIBEOS_ISO": "custom.iso",
                "VIBEOS_SMP": "4",
                "VIBEOS_QEMU_CPU": "qemu64",
                "VIBEOS_MEM": "256M",
                "VIBEOS_BIOS": "/ovmf.fd",
                "VIBEOS_QEMU_ACCEL": "",
                "VIBEOS_TIMEOUT": "12.5",
                "VIBEOS_QEMU_EXTRA": "-nic none",
            },
            clear=True,
        ):
            env = env_config(default_iso="vibeos.iso", default_timeout=60.0)
            self.assertEqual(env.iso, "custom.iso")
            self.assertEqual(env.smp, 4)
            self.assertEqual(env.cpu, "qemu64")
            self.assertEqual(env.mem, "256M")
            self.assertEqual(env.bios, "/ovmf.fd")
            self.assertEqual(env.accel, "")
            self.assertEqual(env.timeout, 12.5)
            self.assertEqual(env.extra, ("-nic", "none"))


class TestMceHelpers(unittest.TestCase):
    DUMP = [
        "vibeOS: #MC rip=0xffffffff80001234 cs=0x8 rflags=0x2 rsp=0x0 ss=0x0",
        "vibeOS: regs: rbp=0x0 rsp=0x0 rflags=0x2 rip=0xffffffff80001234 cr3=0x1000",
        "vibeOS: panic: thread cpu=0 tid=0 idle",
        "vibeOS: backtrace:",
        "vibeOS: panic: halted",
    ]

    def test_command_format(self) -> None:
        cmd = mce_monitor_cmd(
            cpu=0, bank=1, status=MCE_UC_STATUS, mcg_status=MCE_MCG_STATUS
        )
        self.assertEqual(cmd, "mce 0 1 0xb200000000000000 0x5 0x0 0x0")
        cmd = mce_monitor_cmd(
            cpu=3, bank=2, status=1, mcg_status=4, addr=0x1000, misc=0x86
        )
        self.assertEqual(cmd, "mce 3 2 0x1 0x4 0x1000 0x86")

    def test_rejects_values(self) -> None:
        with self.assertRaisesRegex(HarnessError, "cpu"):
            mce_monitor_cmd(cpu=-1, bank=1, status=1, mcg_status=5)
        with self.assertRaisesRegex(HarnessError, "status"):
            mce_monitor_cmd(cpu=0, bank=1, status=1 << 64, mcg_status=5)
        with self.assertRaisesRegex(HarnessError, "misc"):
            mce_monitor_cmd(cpu=0, bank=1, status=1, mcg_status=5, misc=-2)
        mce_monitor_cmd(cpu=0, bank=1, status=(1 << 64) - 1, mcg_status=5)

    def test_passing_dump(self) -> None:
        check_mce_dump(self.DUMP, exit_code=None, reply="")
        check_mce_dump(self.DUMP, exit_code=-9, reply="", needles=MCE_DUMP_NEEDLES)

    def test_qemu_exit_before_dump_carries_reply(self) -> None:
        reply = "mce 0 1 0xb200000000000000 0x5 0x0 0x0 MCE capability is not enabled"
        with self.assertRaises(HarnessError) as cm:
            check_mce_dump(["vibeOS: smp: done"], exit_code=0, reply=reply)
        msg = str(cm.exception)
        self.assertIn("QEMU exited (status 0) before the #MC dump", msg)
        self.assertIn("MCE capability is not enabled", msg)

    def test_no_dump_while_running(self) -> None:
        with self.assertRaisesRegex(HarnessError, "no #MC dump within"):
            check_mce_dump([], exit_code=None, reply="")

    def test_dump_on_wrong_cpu(self) -> None:
        dump = [ln.replace("cpu=0", "cpu=1") for ln in self.DUMP]
        with self.assertRaisesRegex(HarnessError, "joint needle"):
            check_mce_dump(dump, exit_code=None, reply="")

    def test_dump_cut_short(self) -> None:
        with self.assertRaisesRegex(HarnessError, "halted"):
            check_mce_dump(self.DUMP[:-1], exit_code=0, reply="")

    def test_monitor_reply_reads_to_prompt(self) -> None:
        a, b = socket.socketpair()
        with a, b:
            b.sendall(b"mce 0 1 0x1 0x5 0x0 0x0\r\n\x1b[Kbad value\r\n(qemu) ")
            self.assertEqual(
                _monitor_reply(a, 1.0), "bad value"
            )
            b.sendall(b"partial")
            b.close()
            self.assertEqual(_monitor_reply(a, 1.0), "partial")


if __name__ == "__main__":
    unittest.main()
