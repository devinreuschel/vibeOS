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
        argv = _qemu_argv(QemuConfig(iso="x.iso", accel="tcg"), "/tmp/mon")
        i = argv.index("-accel")
        self.assertEqual(argv[i : i + 2], ["-accel", "tcg"])

    def test_accel_kvm_override(self) -> None:
        argv = _qemu_argv(QemuConfig(iso="x.iso", accel="kvm"), "/tmp/mon")
        i = argv.index("-accel")
        self.assertEqual(argv[i : i + 2], ["-accel", "kvm"])

    def test_accel_empty_omits_flag(self) -> None:
        argv = _qemu_argv(QemuConfig(iso="x.iso", accel=""), "/tmp/mon")
        self.assertNotIn("-accel", argv)


class TestLapicMode(unittest.TestCase):
    def test_tcg_max_is_periodic(self) -> None:
        from harness import expected_lapic_mode

        self.assertEqual(
            expected_lapic_mode(cpu="max", hpet=True, accel="tcg"),
            "periodic",
        )

    def test_hpet_off_is_pit(self) -> None:
        from harness import expected_lapic_mode

        self.assertEqual(
            expected_lapic_mode(cpu="max", hpet=False, accel="tcg"),
            "pit",
        )

    def test_kvm_max_is_tsc_deadline(self) -> None:
        from harness import expected_lapic_mode

        self.assertEqual(
            expected_lapic_mode(cpu="max", hpet=True, accel="kvm"),
            "tsc-deadline",
        )

    def test_cpu_flag_disables_deadline(self) -> None:
        from harness import expected_lapic_mode

        self.assertEqual(
            expected_lapic_mode(
                cpu="qemu64,-tsc-deadline", hpet=True, accel="kvm"
            ),
            "periodic",
        )

    def test_boot_contract_pins_mode(self) -> None:
        from harness import boot_contract_markers

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
        from harness import check_dump_needles, dump_after_panic

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
        from harness import contains_panic

        self.assertFalse(contains_panic("dmesg: page fault help text"))
        self.assertTrue(contains_panic("vibeOS: #PF rip=0x1"))

    def test_missing_joint_needle_fails(self) -> None:
        from harness import HarnessError, check_dump_needles

        with self.assertRaises(HarnessError):
            check_dump_needles(
                ["vibeOS: logrec: hello", "vibeOS: smp: done"],
                (("vibeOS: logrec:", "smp: done"),),
            )


if __name__ == "__main__":
    unittest.main()
