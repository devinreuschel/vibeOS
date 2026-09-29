"""Unit tests for the e2e harness itself (DESIGN §8.3).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import os
import socket
import sys
import time
import unittest
from typing import Any
from unittest import mock

import tests.harness.run_e2e as run_e2e
import tests.harness.run_ktest as run_ktest
from tests.harness import frame, results
from tests.harness.harness import (
    AP_ONLINE,
    HPET_OFF_MACHINE,
    ISA_DEBUG_FAIL,
    ISA_DEBUG_PASS,
    MCE_DUMP_NEEDLES,
    MCE_MCG_STATUS,
    MCE_UC_STATUS,
    OVMF_BOOT_ARGS,
    PANIC_EXIT_S,
    PANIC_EXIT_STATUS,
    DeadlineReader,
    HarnessError,
    Marker,
    QemuConfig,
    QemuProcess,
    RunResult,
    _monitor_reply,
    boot_contract_markers,
    check_mce_dump,
    contains_panic,
    drain_panic_tail,
    effective_accel_name,
    halt_test_markers,
    is_dump_banner,
    kernel_text,
    mce_monitor_cmd,
    overlay_env,
    qemu_argv,
    run_qemu_and_check,
    run_qemu_console_input,
    serial_tail,
)
from tests.harness.linesource import FakeLineSource


def K(text: str) -> str:
    """`text` as the kernel prints it on serial: framed (DESIGN §2.6)."""
    return frame.FRAME + text


class TestMarkerShape(unittest.TestCase):
    def test_serial_tail(self) -> None:
        self.assertEqual(serial_tail([]), " (no serial)")
        self.assertIn("b", serial_tail(["a", "b"], n=1))
        self.assertIn("1/2", serial_tail(["a", "b"], n=1))
        self.assertNotIn("\na\n", serial_tail(["a", "b"], n=1))

    def test_and_contains_requires_all_fragments(self) -> None:
        # A line that carries only the suffix must NOT satisfy a marker
        # whose shape includes both `vibeOS: pmm:` and the suffix. This
        # is the phase-1 PMM marker's contract per DESIGN §2.6 / §8.3.
        m = Marker(
            "vibeOS: pmm: ",
            "pmm_free_frames",
            and_contains=(" free 4KiB frames",),
        )
        self.assertTrue(m.matches(K("vibeOS: pmm: 31329 free 4KiB frames")))
        self.assertFalse(m.matches(K("someone reports 12 free 4KiB frames")))
        self.assertFalse(m.matches(K("vibeOS: pmm: initializing")))

    def test_pci_count_marker_ignores_per_device_lines(self) -> None:
        m = Marker(
            "vibeOS: pci: ",
            "pci_devices",
            and_contains=(" devices",),
        )
        self.assertTrue(m.matches(K("vibeOS: pci: 6 devices")))
        self.assertFalse(
            m.matches(K("vibeOS: pci: 00:00.0 8086:1237 host bridge [440FX]"))
        )
        self.assertFalse(m.matches(K("vibeOS: pci: ecam 0xe0000000 buses 0-255")))
        self.assertFalse(m.matches(K("vibeOS: pci: skip bar 00:02.0 size 0x10000000000")))

    def test_block_ramdisk_marker_needs_name_and_sectors(self) -> None:
        m = Marker(
            "vibeOS: block: ",
            "block_ramdisk",
            and_contains=(" ram0 ", " sectors"),
        )
        self.assertTrue(m.matches(K("vibeOS: block: ram0 256 sectors")))
        self.assertFalse(m.matches(K("vibeOS: block: init")))
        self.assertFalse(m.matches(K("vibeOS: block: ram0")))
        self.assertFalse(m.matches(K("ram0 256 sectors")))
        self.assertFalse(m.matches(K("vibeOS: block: ram0p1 32 sectors")))

    def test_block_partition_marker_parent_pN(self) -> None:
        m = Marker(
            "vibeOS: block: ",
            "block_ram0p1",
            and_contains=(" ram0p1 ", " sectors"),
        )
        self.assertTrue(m.matches(K("vibeOS: block: ram0p1 32 sectors")))
        self.assertFalse(m.matches(K("vibeOS: block: ram0 256 sectors")))
        self.assertFalse(m.matches(K("vibeOS: block: ram0p2 24 sectors")))

    def test_block_vda_marker_needs_name_and_sectors(self) -> None:
        m = Marker(
            "vibeOS: block: ",
            "block_vda",
            and_contains=(" vda ", " sectors"),
        )
        self.assertTrue(m.matches(K("vibeOS: block: vda 8192 sectors")))
        self.assertFalse(m.matches(K("vibeOS: block: ram0 256 sectors")))
        self.assertFalse(m.matches(K("vibeOS: block: vda")))
        self.assertFalse(m.matches(K("vibeOS: virtio: blk vda")))
        self.assertFalse(m.matches(K("vibeOS: block: vdap1 128 sectors")))


ABC_MARKERS = [
    Marker("vibeOS: serial online", "a"),
    Marker("vibeOS: limine: rev 3 ok", "b"),
    Marker("vibeOS: boot: phase1 done", "c"),
]
FAKE_CFG = QemuConfig(iso="fake.iso")


def check_fake(
    lines: list[str],
    markers: list[Marker],
    *,
    end: str = "eof",
    exit_code: int | None = 0,
    stderr: str = "",
    **kw: Any,
) -> tuple[RunResult, FakeLineSource]:
    """Run `run_qemu_and_check` over `lines` through a `FakeLineSource`."""
    src = FakeLineSource.from_lines(lines, end=end, exit_code=exit_code, stderr=stderr)
    return run_qemu_and_check(FAKE_CFG, markers, line_source=src, **kw), src


class TestMarkerOrder(unittest.TestCase):
    """`run_qemu_and_check` driven through `FakeLineSource` (F141)."""

    def test_all_present_in_order_quits(self) -> None:
        lines = [
            K("vibeOS: serial online"),
            K("vibeOS: limine: rev 3 ok"),
            K("vibeOS: boot: phase1 done"),
        ]
        result, src = check_fake(lines, ABC_MARKERS)
        self.assertEqual(result.matched, ["a", "b", "c"])
        self.assertTrue(src.quit_sent)
        self.assertEqual(result.exit_code, 0)

    def test_out_of_order_fails(self) -> None:
        lines = [
            K("vibeOS: boot: phase1 done"),  # too early
            K("vibeOS: serial online"),
            K("vibeOS: limine: rev 3 ok"),
        ]
        with self.assertRaises(HarnessError) as cm:
            check_fake(lines, ABC_MARKERS)
        self.assertIn("'c'", str(cm.exception))

    def test_missing_final_marker_fails(self) -> None:
        lines = [K("vibeOS: serial online"), K("vibeOS: limine: rev 3 ok")]
        with self.assertRaises(HarnessError) as cm:
            check_fake(lines, ABC_MARKERS)
        # The error names the missing marker's `name`, not its substring.
        self.assertIn("missing marker 'c'", str(cm.exception))

    def test_panic_signature_fails_fast_and_kills(self) -> None:
        lines = [
            K("vibeOS: serial online"),
            K("panicked at src/foo.rs:1:1"),
            K("vibeOS: boot: phase1 done"),
        ]
        src = FakeLineSource.from_lines(lines)
        with self.assertRaises(HarnessError) as cm:
            run_qemu_and_check(
                FAKE_CFG, [Marker("vibeOS: boot: phase1 done", "c")], line_source=src
            )
        self.assertIn("panicked at", str(cm.exception))
        self.assertTrue(src.killed)
        # Fails at the signature: the line after it is never read.
        self.assertEqual(src.next_event(), ("line", K("vibeOS: boot: phase1 done")))

    def test_and_contains_wrong_shape_fails(self) -> None:
        # The pmm marker must not accept a line that lacks the prefix.
        lines = [
            K("vibeOS: serial online"),
            "diagnostic: 12 free 4KiB frames on some other subsystem",
            K("vibeOS: boot: phase1 done"),
        ]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker(
                K("vibeOS: pmm: "),
                "pmm",
                and_contains=(" free 4KiB frames",),
            ),
            Marker("vibeOS: boot: phase1 done", "b"),
        ]
        with self.assertRaises(HarnessError) as cm:
            check_fake(lines, markers)
        self.assertIn("'pmm'", str(cm.exception))

    def test_extra_lines_between_markers_are_fine(self) -> None:
        lines = [
            "chatter",
            K("vibeOS: serial online"),
            "more chatter",
            K("vibeOS: limine: rev 3 ok"),
            "even more",
            K("vibeOS: boot: phase1 done"),
        ]
        result, _ = check_fake(lines, ABC_MARKERS)
        self.assertEqual(result.matched, ["a", "b", "c"])

    def test_timeout_names_missing_marker(self) -> None:
        src = FakeLineSource.from_lines([K("vibeOS: serial online")], end="timeout")
        with self.assertRaises(HarnessError) as cm:
            run_qemu_and_check(FAKE_CFG, ABC_MARKERS, line_source=src, timeout_s=7.0)
        msg = str(cm.exception)
        self.assertIn("timed out after 7.0s", msg)
        self.assertIn("missing 'b'", msg)
        self.assertTrue(src.killed)

    def test_fake_skips_path_and_iso_checks(self) -> None:
        with mock.patch("shutil.which", return_value=None):
            result, _ = check_fake([K("vibeOS: serial online")], ABC_MARKERS[:1])
        self.assertEqual(result.matched, ["a"])


def contract_log(markers: list[Marker]) -> list[str]:
    """One synthetic serial line per marker, each matching it: framed for a
    kernel marker, unframed for a user program's (`shell ready`)."""
    out = []
    for m in markers:
        text = m.substring + "".join(m.and_contains)
        out.append(text if frame.source_of(m.substring) == frame.USER else K(text))
    return out


def smp_log(n: int, *, extra_before: int = 0, extra_after: int = 0) -> list[str]:
    """A `-smp n` contract log with extra `ap online` lines around `smp: done`."""
    lines = contract_log(boot_contract_markers(smp=n, cpu="max", accel="tcg"))
    i = lines.index(K("vibeOS: smp: done"))
    ap = K(AP_ONLINE)
    return lines[:i] + [ap] * extra_before + [lines[i]] + [ap] * extra_after + (
        lines[i + 1 :]
    )


class TestSmpApCount(unittest.TestCase):
    """`smp: done` needs exactly N-1 `ap online` lines (F141)."""

    def run_smp(self, n: int, lines: list[str]) -> RunResult:
        markers = boot_contract_markers(smp=n, cpu="max", accel="tcg")
        return run_qemu_and_check(
            FAKE_CFG, markers, line_source=FakeLineSource.from_lines(lines)
        )

    def test_smp2_one_line_passes(self) -> None:
        lines = smp_log(2)
        self.assertEqual(lines.count(K(AP_ONLINE)), 1)
        self.assertIn("smp_done", self.run_smp(2, lines).matched)

    def test_smp2_extra_line_before_done_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.run_smp(2, smp_log(2, extra_before=1))
        self.assertIn(
            "extra 'vibeOS: smp: ap online' line (2 seen, expected exactly 1)",
            str(cm.exception),
        )

    def test_smp2_extra_line_after_done_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.run_smp(2, smp_log(2, extra_after=1))
        self.assertIn("(2 seen, expected exactly 1)", str(cm.exception))

    def test_smp1_with_a_line_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.run_smp(1, smp_log(1, extra_before=1))
        self.assertIn("(1 seen, expected exactly 0)", str(cm.exception))

    def test_smp1_without_lines_passes(self) -> None:
        self.assertIn("smp_done", self.run_smp(1, smp_log(1)).matched)

    def test_smp4_three_lines_passes(self) -> None:
        lines = smp_log(4)
        self.assertEqual(lines.count(K(AP_ONLINE)), 3)
        self.assertIn("smp_done", self.run_smp(4, lines).matched)

    def test_too_few_before_owner_fails(self) -> None:
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker("vibeOS: smp: done", "smp_done", exactly_before=(AP_ONLINE, 2)),
        ]
        lines = [K("vibeOS: serial online"), K(AP_ONLINE), K("vibeOS: smp: done")]
        with self.assertRaises(HarnessError) as cm:
            check_fake(lines, markers)
        self.assertIn("before 'smp_done', expected exactly 2", str(cm.exception))


# A real `make test-e2e-panic` boot's serial (the panic_exit build).
PANIC_BOOT = [
    "limine: Loading executable `boot():/boot/vibeos`...",
    K("vibeOS: serial online"),
    K("vibeOS: limine: rev 3 ok"),
    K("vibeOS: boot: panic-test armed"),
]
PANIC_DUMP = [
    K("vibeOS: panic:"),
    K("vibeOS: panic: at src/main.rs:110:9"),
    K("vibeOS: panic: msg: intentional panic-test trip"),
    K("vibeOS: regs: rbp=0xffff800007f92f70 rsp=0xffff800007f92ee0 rflags=0x82"),
    K("vibeOS: panic: thread cpu=0 tid=0 <early>"),
    K("vibeOS: log: last 3 (0 dropped)"),
    K("vibeOS: logrec: 2980393398tsc cpu0 info vibeOS: serial online"),
    K("vibeOS: logrec: 2981355280tsc cpu0 info vibeOS: limine: rev 3 ok"),
    K("vibeOS: logrec: 2981513364tsc cpu0 info vibeOS: boot: panic-test armed"),
    K("vibeOS: backtrace:"),
    K("  0xffffffff80001a5b __rustc::rust_begin_unwind+0x1b"),
    K("vibeOS: panic: halted"),
]
PANIC_NEEDLES: tuple[str | tuple[str, ...], ...] = (
    "vibeOS: panic: at",
    "intentional panic-test",
    ("vibeOS: logrec:", "serial online"),
    "rust_begin_unwind",
    "vibeOS: panic: halted",
)
BANNER_KINDS = (
    K("vibeOS: panic:"),
    K("vibeOS: exception: vector 3 rip=0xffffffff80001000 cs=0x8"),
    K("vibeOS: #UD rip=0xffffffff80001000 cs=0x8"),
    K("vibeOS: nmi rip=0xffffffff80001000 cs=0x8"),
    K("vibeOS: #DB rip=0xffffffff80001000 cs=0x8"),
    K("vibeOS: #GP rip=0xffffffff80001000 cs=0x8 err=0x0"),
    K("vibeOS: #PF rip=0xffffffff80001000 cs=0x8 err=0x2 cr2=0x0"),
    K("vibeOS: #DF rip=0xffffffff80001000 cs=0x8 err=0x0"),
    K("vibeOS: #MC rip=0xffffffff80001000 cs=0x8"),
)


class TestExpectPanic(unittest.TestCase):
    """expect_panic: pre-panic markers, one banner, exit status 35 (F141)."""

    def expect(
        self,
        lines: list[str],
        *,
        markers: list[Marker] | None = None,
        end: str = "eof",
        exit_code: int | None = PANIC_EXIT_STATUS,
        needles: tuple[str | tuple[str, ...], ...] = (),
    ) -> tuple[RunResult, FakeLineSource]:
        src = FakeLineSource.from_lines(lines, end=end, exit_code=exit_code)
        result = run_qemu_and_check(
            FAKE_CFG,
            halt_test_markers() if markers is None else markers,
            expect_panic=True,
            dump_needles=needles,
            line_source=src,
        )
        return result, src

    def test_real_dump_passes_unkilled(self) -> None:
        result, src = self.expect(PANIC_BOOT + PANIC_DUMP, needles=PANIC_NEEDLES)
        self.assertEqual(result.matched, ["serial_online", "limine_ok", "panic_test_armed"])
        self.assertEqual(result.panic_line, K("vibeOS: panic:"))
        self.assertEqual(result.exit_code, 35)
        self.assertFalse(src.killed)
        self.assertFalse(src.quit_sent)

    def test_exit_wait_deadline(self) -> None:
        t0 = time.monotonic()
        _, src = self.expect(PANIC_BOOT + PANIC_DUMP)
        self.assertEqual(len(src.deadlines), 1)
        self.assertGreaterEqual(src.deadlines[0], t0 + PANIC_EXIT_S - 0.1)

    def test_marker_only_in_logrec_replay_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.expect(PANIC_BOOT[:3] + PANIC_DUMP)
        self.assertIn(
            "missing marker 'panic_test_armed' before the first panic signature",
            str(cm.exception),
        )

    def test_count_ignores_logrec_replay(self) -> None:
        markers = [
            Marker("vibeOS: smp: done", "smp_done", exactly_before=(AP_ONLINE, 1)),
        ]
        lines = [K(AP_ONLINE), K("vibeOS: smp: done"), K("vibeOS: #GP rip=0x1 err=0x0")]
        lines += [K(f"vibeOS: logrec: 46ms cpu0 info {AP_ONLINE}"), K("vibeOS: panic: halted")]
        result, _ = self.expect(lines, markers=markers)
        self.assertEqual(result.matched, ["smp_done"])

    def test_exit_zero_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.expect(PANIC_BOOT + PANIC_DUMP, exit_code=0)
        self.assertIn("panic exit status 0, expected 35", str(cm.exception))

    def test_halted_then_timeout_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.expect(PANIC_BOOT + PANIC_DUMP, end="timeout", exit_code=None)
        self.assertIn(
            "QEMU did not exit within 10 s of 'vibeOS: panic: halted'", str(cm.exception)
        )

    def test_dump_ended_before_halted_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.expect(PANIC_BOOT + PANIC_DUMP[:-1], exit_code=1)
        msg = str(cm.exception)
        self.assertIn("dump ended before 'vibeOS: panic: halted'", msg)
        self.assertIn("QEMU exited with status 1", msg)

    def test_two_banners_fail(self) -> None:
        lines = PANIC_BOOT + PANIC_DUMP[:3] + [K("vibeOS: #PF rip=0x1 err=0x0 cr2=0x0")]
        with self.assertRaises(HarnessError) as cm:
            self.expect(lines + PANIC_DUMP[3:])
        self.assertIn("expected one dump banner, saw 2", str(cm.exception))

    def test_reentered_fails(self) -> None:
        lines = PANIC_BOOT + PANIC_DUMP[:3] + [K("vibeOS: panic: reentered")]
        with self.assertRaises(HarnessError) as cm:
            self.expect(lines + [K("vibeOS: panic: halted")])
        self.assertIn("expected one dump banner, saw 2", str(cm.exception))

    def test_signature_without_banner_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.expect(PANIC_BOOT + PANIC_DUMP[1:])
        self.assertIn("expected one dump banner, saw 0", str(cm.exception))

    def test_no_panic_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.expect(PANIC_BOOT)
        self.assertIn("expected a panic signature; none seen", str(cm.exception))

    def test_each_banner_kind_passes(self) -> None:
        for banner in BANNER_KINDS:
            with self.subTest(banner=banner):
                self.assertTrue(is_dump_banner(banner))
                lines = PANIC_BOOT + [banner, K("vibeOS: panic: thread cpu=0 tid=0")]
                result, _ = self.expect(lines + [K("vibeOS: panic: halted")])
                self.assertEqual(result.panic_line, banner)

    def test_non_banners(self) -> None:
        for line in (
            K("vibeOS: logrec: 1ms cpu0 info vibeOS: panic:"),
            K("vibeOS: logrec: 1ms cpu0 info vibeOS: #GP rip=0x1"),
            K("vibeOS: panic: at src/main.rs:1:1"),
            K("vibeOS: panic: halted"),
            K("x vibeOS: panic:"),
            # A user program's copy of a banner is not one.
            "vibeOS: panic:",
            "?vibeOS: #GP rip=0x1 cs=0x8",
        ):
            with self.subTest(line=line):
                self.assertFalse(is_dump_banner(line))


class TestFirstKernelLine(unittest.TestCase):
    """`vibeOS: serial online` is the kernel's first serial line."""

    def boot(self, lines: list[str]) -> None:
        result, _ = check_fake(lines, ABC_MARKERS)
        run_e2e.check_first_kernel_line(result.lines)

    def test_kernel_line_first_fails(self) -> None:
        lines = [K("vibeOS: heap ok")] + [K(m.substring) for m in ABC_MARKERS]
        with self.assertRaises(HarnessError) as cm:
            self.boot(lines)
        self.assertIn(
            "kernel line before 'vibeOS: serial online': vibeOS: heap ok", str(cm.exception)
        )

    def test_limine_line_first_passes(self) -> None:
        self.boot(PANIC_BOOT[:1] + [K(m.substring) for m in ABC_MARKERS])

    def test_nothing_before_serial_line_passes(self) -> None:
        self.boot([K(m.substring) for m in ABC_MARKERS])

    def test_glued_non_kernel_prefix_passes(self) -> None:
        # Loader output that left its line open: the kernel's first line
        # starts on a fresh one, so the loader's text is a line of its own.
        lines = ["\x1b[2J\x1b[Hlimine: boot "] + [K(m.substring) for m in ABC_MARKERS]
        self.boot(lines)

    def test_user_copy_before_serial_line_passes(self) -> None:
        lines = ["vibeOS: heap ok"] + [K(m.substring) for m in ABC_MARKERS]
        self.boot(lines)

    def test_glued_kernel_prefix_fails(self) -> None:
        lines = [K(m.substring) for m in ABC_MARKERS]
        lines[0] = K("vibeOS: heap ok" + ABC_MARKERS[0].substring)
        with self.assertRaises(HarnessError) as cm:
            self.boot(lines)
        self.assertIn("vibeOS: heap okvibeOS: serial online", str(cm.exception))

    def test_no_serial_line_passes_here(self) -> None:
        run_e2e.check_first_kernel_line(["limine: Loading executable"])

    def test_kernel_text(self) -> None:
        self.assertEqual(kernel_text(K("vibeOS: heap ok")), "vibeOS: heap ok")
        self.assertIsNone(kernel_text("vibeOS: heap ok"))
        self.assertIsNone(kernel_text("limine: Loading executable"))
        self.assertIsNone(kernel_text("user: tests ok"))


# The memory diagnostics of a real `make test-e2e` boot.
MEMINFO_BOOT = [
    "limine: Loading executable `boot():/boot/vibeos`...",
    K("vibeOS: serial online"),
    K("vibeOS: limine: rev 3 ok"),
    K("vibeOS: pmm: 29503 free 4KiB frames"),
    K("vibeOS: pmm: 29503 total, largest order 10"),
    K("vibeOS: paging: cr3 ok"),
    K("vibeOS: meminfo: total 29503 frames, free 29208, used 295, largest order 10"),
    K("vibeOS: meminfo: leaked 0 frames"),
    K("vibeOS: meminfo: heap used 5728 B / capacity 1048576 B"),
    K("vibeOS: meminfo: kva used 102400 B"),
    K("vibeOS: pt: 16 ranges"),
    K("vibeOS: sched: cpu0 ready"),
]
MEMINFO_TOTAL_LINE = MEMINFO_BOOT.index(
    K("vibeOS: meminfo: total 29503 frames, free 29208, used 295, largest order 10")
)
MEMINFO_HEAP_LINE = MEMINFO_BOOT.index(K("vibeOS: meminfo: heap used 5728 B / capacity 1048576 B"))


def doctor(i: int, line: str) -> list[str]:
    """`MEMINFO_BOOT` with its line `i` replaced by the kernel line `line`."""
    out = list(MEMINFO_BOOT)
    out[i] = K(line)
    return out


class TestMeminfoCheck(unittest.TestCase):
    """The boot log's `meminfo:` lines agree with its `pmm:` lines."""

    def fails(self, lines: list[str], needle: str) -> None:
        with self.assertRaises(HarnessError) as cm:
            run_e2e.check_meminfo(lines)
        self.assertIn(needle, str(cm.exception))

    def test_real_boot_passes(self) -> None:
        run_e2e.check_meminfo(MEMINFO_BOOT)

    def test_duplicate_total_fails(self) -> None:
        lines = MEMINFO_BOOT + [MEMINFO_BOOT[MEMINFO_TOTAL_LINE]]
        self.fails(lines, "'vibeOS: meminfo: total ' line appears 2 times, expected once")

    def test_duplicate_pmm_line_fails(self) -> None:
        self.fails(MEMINFO_BOOT + [MEMINFO_BOOT[3]], "2 'vibeOS: pmm: <n> free 4KiB frames' lines")

    def test_total_not_pmm_total_fails(self) -> None:
        line = "vibeOS: meminfo: total 29504 frames, free 29209, used 295, largest order 10"
        self.fails(doctor(MEMINFO_TOTAL_LINE, line), "total 29504 frames, but pmm: 29503 total")

    def test_free_above_pmm_free_fails(self) -> None:
        lines = doctor(3, "vibeOS: pmm: 29000 free 4KiB frames")
        self.fails(lines, "free 29208 above pmm: 29000 free 4KiB frames")

    def test_used_not_total_minus_free_fails(self) -> None:
        line = "vibeOS: meminfo: total 29503 frames, free 29208, used 296, largest order 10"
        self.fails(doctor(MEMINFO_TOTAL_LINE, line), "used 296 is not total 29503 minus free 29208")

    def test_heap_above_capacity_fails(self) -> None:
        line = "vibeOS: meminfo: heap used 1048577 B / capacity 1048576 B"
        self.fails(doctor(MEMINFO_HEAP_LINE, line), "heap used 1048577 B above capacity 1048576 B")

    def test_no_meminfo_line_fails(self) -> None:
        lines = [ln for ln in MEMINFO_BOOT if "meminfo:" not in ln]
        self.fails(lines, "0 'vibeOS: meminfo: total' lines, expected one")

    def test_trailing_fields_tolerated(self) -> None:
        line = f"{kernel_text(MEMINFO_BOOT[MEMINFO_TOTAL_LINE])}, leaked 0"
        run_e2e.check_meminfo(doctor(MEMINFO_TOTAL_LINE, line))


QEMU_LOAD_ERR = "qemu-system-x86_64: -bios x.fd: could not load"


class TestQemuExitReport(unittest.TestCase):
    """An early QEMU exit names its status and stderr (F079)."""

    def test_fake_exit_names_status_and_stderr(self) -> None:
        src = FakeLineSource([], exit_code=1, stderr=QEMU_LOAD_ERR + "\n")
        with self.assertRaises(HarnessError) as cm:
            run_qemu_and_check(FAKE_CFG, ABC_MARKERS[:1], line_source=src)
        msg = str(cm.exception)
        self.assertIn("missing marker 'a' after 0 lines", msg)
        self.assertIn("QEMU exited with status 1", msg)
        self.assertIn("--- qemu stderr ---", msg)
        self.assertIn(QEMU_LOAD_ERR, msg)

    def test_no_stderr_says_so(self) -> None:
        src = FakeLineSource.from_lines(["limine: Loading executable"], exit_code=0)
        with self.assertRaises(HarnessError) as cm:
            run_qemu_and_check(FAKE_CFG, ABC_MARKERS[:1], line_source=src)
        msg = str(cm.exception)
        self.assertIn("status 0", msg)
        self.assertIn("(no stderr)", msg)
        self.assertIn("limine: Loading executable", msg)

    def test_stderr_tail_is_last_20_lines(self) -> None:
        err = "".join(f"warn {i}\n" for i in range(30))
        src = FakeLineSource([], exit_code=1, stderr=err)
        with self.assertRaises(HarnessError) as cm:
            run_qemu_and_check(FAKE_CFG, ABC_MARKERS[:1], line_source=src)
        msg = str(cm.exception)
        self.assertIn("warn 29", msg)
        self.assertIn("warn 10", msg)
        self.assertNotIn("warn 9\n", msg)

    def test_timeout_shows_nonempty_stderr(self) -> None:
        src = FakeLineSource([("timeout", "")], exit_code=None, stderr="qemu: warning x\n")
        with self.assertRaises(HarnessError) as cm:
            run_qemu_and_check(FAKE_CFG, ABC_MARKERS[:1], line_source=src)
        self.assertIn("qemu: warning x", str(cm.exception))
        src = FakeLineSource([("timeout", "")], exit_code=None)
        with self.assertRaises(HarnessError) as cm:
            run_qemu_and_check(FAKE_CFG, ABC_MARKERS[:1], line_source=src)
        self.assertNotIn("qemu stderr", str(cm.exception))

    def test_console_input_exit_report(self) -> None:
        src = FakeLineSource([], exit_code=1, stderr=QEMU_LOAD_ERR)
        with self.assertRaises(HarnessError) as cm:
            run_qemu_console_input(FAKE_CFG, line_source=src)
        msg = str(cm.exception)
        self.assertIn("status 1", msg)
        self.assertIn(QEMU_LOAD_ERR, msg)

    def test_real_child_stderr_kept_apart_from_serial(self) -> None:
        argv = [
            sys.executable,
            "-c",
            "import sys; print('serial line'); "
            "sys.stderr.write('qemu-system-x86_64: could not load\\n'); sys.exit(1)",
        ]
        src = QemuProcess(argv, time.monotonic() + 10.0)
        with self.assertRaises(HarnessError) as cm:
            run_qemu_and_check(FAKE_CFG, ABC_MARKERS[:1], line_source=src)
        msg = str(cm.exception)
        self.assertIn("missing marker 'a' after 1 lines", msg)
        self.assertIn("QEMU exited with status 1", msg)
        self.assertIn("--- qemu stderr ---\nqemu-system-x86_64: could not load", msg)
        self.assertIn("--- serial tail 1/1 ---\nserial line", msg)


CONSOLE_OK_LINES = [
    "vibeOS: shell ready",
    "$ echo serial-ok",
    "serial-ok",
    "$ echo ps2-ok",
    "ps2-ok",
]


class TestConsoleInput(unittest.TestCase):
    """`run_qemu_console_input` driven through `FakeLineSource`."""

    def test_serial_then_sendkey_then_quit(self) -> None:
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES, end="timeout")
        result = run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertEqual(result.matched, ["shell_ready", "serial_echo", "ps2_echo"])
        self.assertEqual(src.inputs, [b"echo serial-ok\n"])
        self.assertEqual(len(src.monitor_cmds), 1)
        self.assertTrue(src.monitor_cmds[0].startswith("sendkey e-c-h-o-spc-p-s-2"))
        self.assertTrue(src.quit_sent)

    def test_missing_ps2_echo_fails(self) -> None:
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES[:3])
        with self.assertRaises(HarnessError) as cm:
            run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertIn("PS/2 sendkey echo missing", str(cm.exception))

    def test_panic_fails(self) -> None:
        src = FakeLineSource.from_lines(["vibeOS: shell ready", K("vibeOS: panic: x")])
        with self.assertRaises(HarnessError) as cm:
            run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertIn("vibeOS: panic:", str(cm.exception))
        self.assertTrue(src.killed)


class TestConsoleTail(unittest.TestCase):
    """The console boot reads serial 3 s past its last reply."""

    def test_panic_after_last_reply_fails(self) -> None:
        src = FakeLineSource.from_lines(
            CONSOLE_OK_LINES + [K("vibeOS: vibefs: commit"), K("vibeOS: panic:")], end="timeout"
        )
        with self.assertRaises(HarnessError) as cm:
            run_qemu_console_input(FAKE_CFG, line_source=src)
        msg = str(cm.exception)
        self.assertIn("in the 3.0 s after the last reply: vibeOS: panic:", msg)
        self.assertTrue(src.killed)
        self.assertFalse(src.quit_sent)

    def test_extra_panic_counts_in_tail(self) -> None:
        cfg = QemuConfig(iso="fake.iso", extra_panic=("vibeOS: sched: overdue",))
        src = FakeLineSource.from_lines(
            CONSOLE_OK_LINES + [K("vibeOS: sched: overdue tid 7")], end="timeout"
        )
        with self.assertRaises(HarnessError) as cm:
            run_qemu_console_input(cfg, line_source=src)
        self.assertIn("overdue tid 7", str(cm.exception))

    def test_timeout_passes_and_quits(self) -> None:
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES + ["chatter"], end="timeout")
        t0 = time.monotonic()
        result = run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertTrue(src.quit_sent)
        self.assertFalse(src.killed)
        self.assertEqual(result.lines[-1], "chatter")
        self.assertEqual(len(src.deadlines), 1)
        self.assertGreaterEqual(src.deadlines[0], t0 + 2.9)

    def test_eof_in_tail_fails_with_status(self) -> None:
        src = FakeLineSource.from_lines(
            CONSOLE_OK_LINES, end="eof", exit_code=3, stderr="qemu: gone"
        )
        with self.assertRaises(HarnessError) as cm:
            run_qemu_console_input(FAKE_CFG, line_source=src)
        msg = str(cm.exception)
        self.assertIn("QEMU exited in the 3.0 s after the last reply", msg)
        self.assertIn("status 3", msg)
        self.assertIn("qemu: gone", msg)


class TestPanicSignatureScan(unittest.TestCase):
    def test_matches_exception_mnemonic(self) -> None:
        self.assertTrue(contains_panic(K("cpu halted on #PF at ...")))
        self.assertTrue(contains_panic(K("panicked at src/main.rs:12:5")))

    def test_english_prose_is_not_a_false_positive(self) -> None:
        # DESIGN §9.7: matching prose is a footgun. `page fault` must NOT
        # trigger the scanner; only `#PF` does.
        self.assertFalse(contains_panic(K("shell help: 'demo a page fault'")))
        self.assertFalse(contains_panic(K("help text about general protection")))

    def test_double_fault_phrase_matches_intentionally(self) -> None:
        # The literal phrase 'double fault' is in the harness signature list.
        # If someone puts it in help text later, they need to rename the
        # help text, not the harness.
        self.assertTrue(contains_panic(K("we hit a double fault")))


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
                b"\x1emsg: ipi: ack timeout waiters=0xd\n"
                b"vibeOS: panic: halted\n"
                b"\x1evibeOS: panic: halted\n"
                b"ignored\n",
            )
            os.close(w)
            w = -1
            result = RunResult(lines=[K("vibeOS: panic:")])
            reader = DeadlineReader(r, time.monotonic() + 1.0)
            drain_panic_tail(reader, result, window_s=0.5)
            # A user program's `panic: halted` does not end the drain.
            self.assertEqual(
                result.lines,
                [
                    K("vibeOS: panic:"),
                    K("msg: ipi: ack timeout waiters=0xd"),
                    "vibeOS: panic: halted",
                    K("vibeOS: panic: halted"),
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
            K("vibeOS: ktest: begin"),
            K("vibeOS: ktest: ok map_unmap"),
            K("vibeOS: ktest: end"),
        ]
        check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_fail_line_rejected(self) -> None:
        from tests.harness.harness import ISA_DEBUG_PASS, HarnessError, check_ktest_output

        lines = [
            K("vibeOS: ktest: begin"),
            K("vibeOS: ktest: FAIL nx_enforcement: PF was not instruction-fetch"),
            K("vibeOS: ktest: end"),
        ]
        with self.assertRaises(HarnessError) as cm:
            check_ktest_output(lines, ISA_DEBUG_PASS)
        self.assertIn("FAIL", str(cm.exception))
        self.assertIn("instruction-fetch", str(cm.exception))

    def test_missing_begin_or_end(self) -> None:
        from tests.harness.harness import ISA_DEBUG_PASS, HarnessError, check_ktest_output

        with self.assertRaises(HarnessError):
            check_ktest_output([K("vibeOS: ktest: end")], ISA_DEBUG_PASS)
        with self.assertRaises(HarnessError):
            check_ktest_output([K("vibeOS: ktest: begin")], ISA_DEBUG_PASS)

    def test_wrong_exit_status(self) -> None:
        from tests.harness.harness import ISA_DEBUG_FAIL, HarnessError, check_ktest_output

        lines = [K("vibeOS: ktest: begin"), K("vibeOS: ktest: end")]
        with self.assertRaises(HarnessError) as cm:
            check_ktest_output(lines, ISA_DEBUG_FAIL)
        self.assertIn("isa-debug-exit", str(cm.exception))


class TestNoRetry(unittest.TestCase):
    """Every driver boots each configuration once (ROADMAP §10.2, F021)."""

    @staticmethod
    def _passing_first_boot() -> RunResult:
        return RunResult(
            lines=[
                K("vibeOS: block: vda 8192 sectors"),
                K("vibeOS: block: vdap1 128 sectors"),
                K("vibeOS: block: vdap2 7647 sectors"),
                K("vibeOS: persist: wrote"),
                K("vibeOS: ktest: begin"),
                K("vibeOS: ktest: end"),
            ],
            exit_code=ISA_DEBUG_PASS,
        )

    def _ktest_boot_once(
        self,
        side_effect: HarnessError | RunResult,
        *,
        smp: int = 2,
        persist_reboot: bool = False,
    ) -> None:
        cfg = QemuConfig(iso="x.iso", smp=smp, extra=("-accel", "tcg"))
        with mock.patch.object(
            run_ktest, "run_qemu_until_exit", side_effect=[side_effect]
        ) as run:
            with self.assertRaises(HarnessError):
                run_ktest._ktest_boot(cfg, timeout=1.0, persist_reboot=persist_reboot)
        self.assertEqual(run.call_count, 1)

    def test_dup_ok_timeout_boots_once(self) -> None:
        self._ktest_boot_once(
            HarnessError(
                "timed out after 90.0s; 101 lines"
                "\n--- serial tail 40/101 ---\n"
                "user: tests begin\n"
                "user: dup ok"
            )
        )

    def test_fail_line_boots_once(self) -> None:
        failed = RunResult(
            lines=[
                K("vibeOS: ktest: begin"),
                K("vibeOS: ktest: FAIL msix_cpu: ap counter"),
                K("vibeOS: ktest: end"),
            ],
            exit_code=ISA_DEBUG_FAIL,
        )
        self._ktest_boot_once(failed, smp=4)

    def test_smp4_ipi_ack_panic_boots_once(self) -> None:
        err = HarnessError(
            "panic signature 'vibeOS: panic:' in: "
            "'vibeOS: panic: msg: ipi: ack timeout waiters=0xd'"
        )
        for persist_reboot in (False, True):
            with self.subTest(persist_reboot=persist_reboot):
                self._ktest_boot_once(err, smp=4, persist_reboot=persist_reboot)

    def test_pass_returns_after_one_boot(self) -> None:
        passed = self._passing_first_boot()
        cfg = QemuConfig(iso="x.iso", smp=4, extra=("-accel", "tcg"))
        with mock.patch.object(
            run_ktest, "run_qemu_until_exit", side_effect=[passed]
        ) as run:
            result = run_ktest._ktest_boot(cfg, timeout=1.0, persist_reboot=False)
        self.assertIs(result, passed)
        self.assertEqual(run.call_count, 1)

    def test_e2e_marker_timeout_boots_once(self) -> None:
        with (
            overlay_env({"VIBEOS_ISO": "x.iso"}, clear=True),
            mock.patch.object(results.Results, "write"),
            mock.patch.object(
                run_e2e,
                "run_qemu_and_check",
                side_effect=[HarnessError("timed out after 60.0s; 3 lines")],
            ) as run,
        ):
            self.assertEqual(run_e2e.main(), 1)
        self.assertEqual(run.call_count, 1)

    def test_e2e_console_input_no_shell_boots_once(self) -> None:
        marker_boot = RunResult(
            lines=[
                *MEMINFO_BOOT,
                *[K(f"vibeOS: pci: {id_}") for id_ in run_e2e.PCI_GOLDEN],
                K("vibeOS: pci: 6 devices"),
                *[ln for ln in run_e2e.FORGED_LINES for _ in range(2)],
            ],
            exit_code=0,
        )
        with (
            overlay_env({"VIBEOS_ISO": "x.iso"}, clear=True),
            mock.patch.object(results.Results, "write"),
            mock.patch.object(run_e2e, "run_qemu_and_check", side_effect=[marker_boot]),
            mock.patch.object(
                run_e2e,
                "run_qemu_console_input",
                side_effect=[HarnessError("no shell ready after 60.0s")],
            ) as inp,
        ):
            self.assertEqual(run_e2e.main(), 1)
        self.assertEqual(inp.call_count, 1)


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
            K("vibeOS: smp: done"),
            K("vibeOS: #GP rip=0x1"),
            K("vibeOS: logrec: 3ms cpu0 info vibeOS: smp: done"),
            K("vibeOS: backtrace:"),
            K("vibeOS: panic: halted"),
        ]
        dump = dump_after_panic(lines)
        self.assertEqual(dump[0], K("vibeOS: #GP rip=0x1"))
        check_dump_needles(
            dump,
            ("#GP", "vibeOS: backtrace:", ("vibeOS: logrec:", "smp: done")),
        )

    def test_english_page_fault_still_ignored(self) -> None:
        from tests.harness.harness import contains_panic

        self.assertFalse(contains_panic(K("dmesg: page fault help text")))
        self.assertTrue(contains_panic(K("vibeOS: #PF rip=0x1")))
        # A user program's copy is never a kernel panic.
        self.assertFalse(contains_panic("vibeOS: #PF rip=0x1"))

    def test_missing_joint_needle_fails(self) -> None:
        from tests.harness.harness import HarnessError, check_dump_needles

        with self.assertRaises(HarnessError):
            check_dump_needles(
                [K("vibeOS: logrec: hello"), K("vibeOS: smp: done")],
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

    def test_virtio_blk_nbd_drive_per_cache_mode(self) -> None:
        from tests.harness.harness import virtio_blk_args

        for mode in ("writeback", "none", "writethrough"):
            blob = " ".join(virtio_blk_args("/s/sock", 2, nbd=True, cache=mode))
            self.assertIn(
                "file.driver=nbd,file.server.type=unix,file.server.path=/s/sock", blob
            )
            self.assertIn("format=raw", blob)
            self.assertIn(f"cache={mode}", blob)
            self.assertIn("write-cache=on", blob)
            self.assertNotIn("discard", blob)
        self.assertNotIn("write-cache", " ".join(virtio_blk_args("/d", 2)))

    def test_virtio_blk_rejects_unsafe_cache(self) -> None:
        from tests.harness.harness import virtio_blk_args

        with self.assertRaises(HarnessError):
            virtio_blk_args("/s/sock", 2, nbd=True, cache="unsafe")

    def test_kill_delay_window(self) -> None:
        import random

        from tests.harness.harness import CRASH_KILL_MAX_S, kill_delay

        self.assertEqual(CRASH_KILL_MAX_S, 0.05)
        rng = random.Random(0)
        for _ in range(256):
            d = kill_delay(rng)
            self.assertGreaterEqual(d, 0.0)
            self.assertLessEqual(d, CRASH_KILL_MAX_S)

    def test_make_disk_in_directory(self) -> None:
        import tempfile

        from tests.harness.harness import make_disk

        with tempfile.TemporaryDirectory() as d:
            p = make_disk(8192, "t-", directory=d)
            self.assertEqual(os.path.dirname(p), d)
            self.assertEqual(os.path.getsize(p), 8192)

    def test_run_until_exit_drains_after_harness_kill(self) -> None:
        import signal
        import tempfile

        from tests.harness.harness import overlay_env, run_qemu_until_exit

        with tempfile.TemporaryDirectory() as d:
            qemu = os.path.join(d, "qemu-system-x86_64")
            with open(qemu, "w", encoding="utf-8") as f:
                f.write(
                    "#!/bin/sh\n"
                    "echo 'vibeOS: vibefs: wr 1'\n"
                    "sleep 0.05\n"
                    "echo 'vibeOS: vibefs: wr 2'\n"
                    "exec sleep 30\n"
                )
            os.chmod(qemu, 0o755)
            iso = os.path.join(d, "x.iso")
            open(iso, "wb").close()

            def kill_after(line: str) -> float | None:
                if line.endswith("wr 1"):
                    time.sleep(0.3)
                    return 0.0
                return None

            path = d + os.pathsep + os.environ.get("PATH", "")
            with overlay_env({"PATH": path}):
                r = run_qemu_until_exit(
                    QemuConfig(iso=iso, accel=""), timeout_s=20, kill_after=kill_after
                )
        self.assertIn("vibeOS: vibefs: wr 2", r.lines)
        self.assertEqual(r.exit_code, -signal.SIGKILL)


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
            self.assertEqual(env.qemu_version, "")
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
                "VIBEOS_QEMU_VERSION": "10.2.1",
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
            self.assertEqual(env.qemu_version, "10.2.1")
            self.assertEqual(env.qemu().qemu_version, "10.2.1")


class TestQemuVersionPin(unittest.TestCase):
    """ROADMAP §10.1: a Linux CI job fails when its QEMU is not the pinned one."""

    LINE = "QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3)"

    def setUp(self) -> None:
        from tests.harness import harness

        self._saved = dict(harness._QEMU_VERSIONS)
        harness._QEMU_VERSIONS.clear()
        self.calls: list[str] = []

    def tearDown(self) -> None:
        from tests.harness import harness

        harness._QEMU_VERSIONS.clear()
        harness._QEMU_VERSIONS.update(self._saved)

    def version(self, line: str) -> Any:
        def f(binary: str) -> str:
            self.calls.append(binary)
            return line

        return f

    def pin(
        self,
        pin: str | None,
        *,
        env: dict[str, str] | None = None,
        platform: str = "linux",
        line: str = LINE,
    ) -> None:
        from tests.harness.harness import ensure_qemu_pinned

        ensure_qemu_pinned(
            "qemu-system-x86_64",
            pin,
            env={"CI": "true"} if env is None else env,
            platform=platform,
            version_line=self.version(line),
        )

    def test_match_passes_and_parses_ubuntu_line(self) -> None:
        self.pin("10.2.1")
        self.assertEqual(self.calls, ["qemu-system-x86_64"])

    def test_mismatch_raises_naming_both(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.pin("10.2.1", line="QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1)")
        self.assertIn("8.2.2", str(cm.exception))
        self.assertIn("10.2.1", str(cm.exception))

    def test_empty_pin_raises(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.pin("")
        self.assertIn("VIBEOS_QEMU_VERSION", str(cm.exception))
        self.assertEqual(self.calls, [])

    def test_skips_without_ci_on_darwin_and_without_pin(self) -> None:
        self.pin("1.0.0", env={})
        self.pin("1.0.0", env={"CI": ""})
        self.pin("1.0.0", platform="darwin")
        self.pin(None)
        self.assertEqual(self.calls, [])

    def test_memoized_per_binary(self) -> None:
        self.pin("10.2.1")
        self.pin("10.2.1")
        self.assertEqual(self.calls, ["qemu-system-x86_64"])
        with self.assertRaises(HarnessError):
            self.pin("10.2.2")
        self.assertEqual(self.calls, ["qemu-system-x86_64"])

    def test_qemu_argv_checks_the_config_pin(self) -> None:
        from tests.harness import harness

        cfg = harness.QemuConfig(iso="x.iso", qemu_version="10.2.1")
        with mock.patch.object(harness, "ensure_qemu_pinned") as ens:
            argv = harness.qemu_argv(cfg, None)
        ens.assert_called_once_with("qemu-system-x86_64", "10.2.1")
        self.assertEqual(argv[0], "qemu-system-x86_64")

    def test_plain_config_is_not_checked(self) -> None:
        from tests.harness import harness

        with harness.overlay_env({"CI": "true"}):
            harness.qemu_argv(harness.QemuConfig(iso="x.iso"), None)


class TestMceHelpers(unittest.TestCase):
    DUMP = [
        K("vibeOS: #MC rip=0xffffffff80001234 cs=0x8 rflags=0x2 rsp=0x0 ss=0x0"),
        K("vibeOS: regs: rbp=0x0 rsp=0x0 rflags=0x2 rip=0xffffffff80001234 cr3=0x1000"),
        K("vibeOS: panic: thread cpu=0 tid=0 idle"),
        K("vibeOS: backtrace:"),
        K("vibeOS: panic: halted"),
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
            check_mce_dump([K("vibeOS: smp: done")], exit_code=0, reply=reply)
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
