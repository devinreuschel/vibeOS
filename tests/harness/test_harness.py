"""Unit tests for the e2e harness itself (DESIGN §8.3).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import dataclasses
import os
import platform
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from collections.abc import Iterator
from contextlib import contextmanager
from typing import Any
from unittest import mock

import tests.harness.run_e2e as run_e2e
import tests.harness.run_ktest as run_ktest
from tests.harness import frame, results
from tests.harness.harness import (
    AARCH64_FORENSICS,
    AP_ONLINE,
    FORENSICS_DEVICES,
    HPET_OFF_MACHINE,
    ISA_DEBUG_FAIL,
    ISA_DEBUG_PASS,
    MCE_DUMP_NEEDLES,
    MCE_MCG_STATUS,
    MCE_UC_STATUS,
    OVMF_BOOT_ARGS,
    PANIC_EXIT_S,
    VBLK_BAD_SECTOR,
    VBLK_PATTERN_XOR,
    DeadlineReader,
    EnvConfig,
    Firmware,
    HarnessError,
    Marker,
    QemuConfig,
    QemuProcess,
    RunResult,
    _monitor_reply,
    boot_contract_markers,
    check_mce_dump,
    contains_panic,
    core_report,
    effective_accel_name,
    failure_tail,
    halt_test_markers,
    is_dump_banner,
    iter_lines_with_deadline,
    kernel_text,
    ktest_devices,
    make_pattern_disk,
    mce_monitor_cmd,
    overlay_env,
    qemu_argv,
    run_qemu_and_check,
    run_qemu_console_input,
    serial_tail,
    sh_power_command,
    virtio_blk_args,
    write_blkdebug_config,
)
from tests.harness.linesource import FakeLineSource
from tests.harness.qmp import FakeQmp


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
        self.assertFalse(m.matches(K("vibeOS: block: ram0p5 24 sectors")))

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
    Marker("vibeOS: limine: rev 6 ok", "b"),
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


class TestCoreReport(unittest.TestCase):
    """The core tool's report after a failed run's serial tail (ROADMAP
    §10.7): `failure_tail` and `core_report`."""

    def test_failure_tail_puts_report_after_serial_tail(self) -> None:
        r = RunResult(lines=["a", "last serial line"])
        r.report = "\n--- core report ---\nsig: timeout @ x < y < z"
        tail = failure_tail(r)
        self.assertIn("--- serial tail 2/2 ---", tail)
        self.assertLess(tail.index("last serial line"), tail.index("--- core report ---"))
        self.assertTrue(tail.endswith("sig: timeout @ x < y < z"))
        self.assertEqual(failure_tail(RunResult()), " (no serial)")

    def test_core_report_names_missing_tool(self) -> None:
        with tempfile.TemporaryDirectory() as d, overlay_env(
            {"VIBEOS_VMCORE": os.path.join(d, "no-such-vmcore")}
        ):
            msg = core_report(os.path.join(d, "core.zst"), os.path.join(d, "kernel.elf"))
        want = "no vmcore tool (run `make vmcore`, or set VIBEOS_VMCORE)"
        self.assertEqual(msg, f"\n--- no core report: {want} ---")

    def test_core_report_pipes_zstd(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            # A stub `zstd -dc -- <file>` that prints the file, and a stub
            # tool that prints its argv and the core it read on stdin.
            zstd = os.path.join(d, "zstd")
            with open(zstd, "w", encoding="utf-8") as f:
                f.write('#!/bin/sh\n[ "$1" = -dc ] && [ "$2" = -- ] || exit 9\nexec cat "$3"\n')
            tool = os.path.join(d, "vmcore")
            with open(tool, "w", encoding="utf-8") as f:
                f.write('#!/bin/sh\necho "args: $*"\necho "stdin: $(cat)"\n')
            for p in (zstd, tool):
                os.chmod(p, 0o755)
            core = os.path.join(d, "core.zst")
            with open(core, "w", encoding="utf-8") as f:
                f.write("CORE-BYTES")
            elf = os.path.join(d, "kernel.elf")
            with open(elf, "w", encoding="utf-8") as f:
                f.write("ELF")
            path = d + os.pathsep + os.environ.get("PATH", "")
            with overlay_env({"PATH": path, "VIBEOS_VMCORE": tool}):
                msg = core_report(core, elf)
                missing = core_report(core, os.path.join(d, "none.elf"))
        self.assertTrue(msg.startswith("\n--- core report ---\n"), msg)
        self.assertIn(f"args: report --elf {elf} --core -", msg)
        self.assertIn("stdin: CORE-BYTES", msg)
        self.assertIn("no kernel ELF at", missing)


class TestVirtioBlkDriveOptions(unittest.TestCase):
    """`virtio_blk_args`' `readonly` and `blkdebug` options and the files
    the two single-test virtio-blk boots use (ROADMAP §10.11)."""

    def test_default_drive_unchanged(self) -> None:
        self.assertEqual(
            virtio_blk_args("/tmp/d.img", 2),
            (
                "-drive",
                "file=/tmp/d.img,if=none,id=vibehd,format=raw,cache=writeback,discard=unmap",
                "-device",
                "virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues=2",
            ),
        )
        self.assertEqual(
            ktest_devices("/tmp/d.img", 2),
            ktest_devices("/tmp/d.img", 2, readonly=False, blkdebug=None),
        )

    def test_readonly_drive(self) -> None:
        args = virtio_blk_args("/tmp/d.img", 2, readonly=True, extra=("/tmp/e.img",))
        self.assertEqual(
            args[1], "file=/tmp/d.img,if=none,id=vibehd,format=raw,cache=writeback,readonly=on"
        )
        # Only the first drive is read-only.
        self.assertIn("discard=unmap", args[5])
        self.assertNotIn("readonly", args[5])
        self.assertIn(args[1], ktest_devices("/tmp/d.img", 2, readonly=True))

    def test_blkdebug_drive(self) -> None:
        args = virtio_blk_args("/tmp/d.img", 1, blkdebug="/tmp/bd.conf")
        self.assertEqual(
            args[1],
            "file=blkdebug:/tmp/bd.conf:/tmp/d.img,if=none,id=vibehd,format=raw,"
            "cache=writeback,discard=unmap,rerror=report,werror=report",
        )
        self.assertIn(args[1], ktest_devices("/tmp/d.img", 1, blkdebug="/tmp/bd.conf"))
        with self.assertRaises(HarnessError):
            virtio_blk_args("/tmp/a:b.img", 1, blkdebug="/tmp/bd.conf")
        with self.assertRaises(HarnessError):
            virtio_blk_args("/tmp/sock", 1, nbd=True, readonly=True)

    def test_blkdebug_config(self) -> None:
        path = write_blkdebug_config(VBLK_BAD_SECTOR, "vibeos-test-bd-")
        try:
            self.assertNotIn(":", path)
            with open(path) as f:
                text = f.read()
        finally:
            os.unlink(path)
        self.assertEqual(
            text,
            "[inject-error]\n"
            'event = "read_aio"\n'
            'iotype = "read"\n'
            'errno = "5"\n'
            'sector = "4096"\n'
            'once = "off"\n'
            'immediately = "off"\n',
        )

    def test_pattern_disk(self) -> None:
        self.assertEqual(VBLK_PATTERN_XOR, 0xA5)
        path = make_pattern_disk(3 * 512, "vibeos-test-pat-")
        try:
            with open(path, "rb") as f:
                data = f.read()
        finally:
            os.unlink(path)
        self.assertEqual(data, b"\xa5" * 512 + b"\xa4" * 512 + b"\xa7" * 512)
        with self.assertRaises(HarnessError):
            make_pattern_disk(100, "vibeos-test-pat-")


class TestMarkerOrder(unittest.TestCase):
    """`run_qemu_and_check` driven through `FakeLineSource` (F141)."""

    def test_all_present_in_order_quits(self) -> None:
        lines = [
            K("vibeOS: serial online"),
            K("vibeOS: limine: rev 6 ok"),
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
            K("vibeOS: limine: rev 6 ok"),
        ]
        with self.assertRaises(HarnessError) as cm:
            check_fake(lines, ABC_MARKERS)
        self.assertIn("'c'", str(cm.exception))

    def test_missing_final_marker_fails(self) -> None:
        lines = [K("vibeOS: serial online"), K("vibeOS: limine: rev 6 ok")]
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
        # Fails at the signature, and reads on only for the dump the core
        # waits on (`vibeOS: panic: halted`): no marker after it matches.
        self.assertEqual(src.next_event(), ("eof", ""))

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
            K("vibeOS: limine: rev 6 ok"),
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


# A real `make test-e2e-panic` boot's serial.
PANIC_BOOT = [
    "limine: Loading executable `boot():/boot/vibeos`...",
    K("vibeOS: serial online"),
    K("vibeOS: limine: rev 6 ok"),
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
    K("vibeOS: logrec: 2981355280tsc cpu0 info vibeOS: limine: rev 6 ok"),
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
    """expect="panic": pre-panic markers, one banner, then QMP
    `GUEST_PANICKED` (F141, ROADMAP §10.7)."""

    def expect(
        self,
        lines: list[str],
        *,
        markers: list[Marker] | None = None,
        end: str = "eof",
        exit_code: int | None = 0,
        needles: tuple[str | tuple[str, ...], ...] = (),
        event: bool = True,
    ) -> tuple[RunResult, FakeLineSource]:
        src = FakeLineSource.from_lines(lines, end=end, exit_code=exit_code)
        # The kernel's pvpanic write follows its last line.
        after: dict[int, list[dict[str, object]]] = {
            len(lines): [{"event": "GUEST_PANICKED", "data": {"action": "pause"}}]
        }
        result = run_qemu_and_check(
            dataclasses.replace(FAKE_CFG, expect="panic"),
            halt_test_markers() if markers is None else markers,
            dump_needles=needles,
            line_source=src,
            qmp=FakeQmp([], after_line=after if event else None),
        )
        return result, src

    def test_real_dump_passes_unkilled(self) -> None:
        result, src = self.expect(PANIC_BOOT + PANIC_DUMP, needles=PANIC_NEEDLES)
        self.assertEqual(result.matched, ["serial_online", "limine_ok", "panic_test_armed"])
        self.assertEqual(result.panic_line, K("vibeOS: panic:"))
        self.assertEqual(result.end, "GUEST_PANICKED")
        self.assertFalse(src.killed)
        self.assertFalse(src.quit_sent)

    def test_exit_wait_deadline(self) -> None:
        t0 = time.monotonic()
        _, src = self.expect(PANIC_BOOT + PANIC_DUMP)
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

    def test_exit_without_event_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.expect(PANIC_BOOT + PANIC_DUMP, event=False)
        self.assertIn("panic run ended without QMP GUEST_PANICKED", str(cm.exception))

    def test_halted_then_timeout_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.expect(PANIC_BOOT + PANIC_DUMP, end="timeout", exit_code=None, event=False)
        self.assertIn(
            "no GUEST_PANICKED within 10 s of 'vibeOS: panic: halted'", str(cm.exception)
        )

    def test_dump_ended_before_halted_fails(self) -> None:
        with self.assertRaises(HarnessError) as cm:
            self.expect(PANIC_BOOT + PANIC_DUMP[:-1], exit_code=1, event=False)
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
    K("vibeOS: limine: rev 6 ok"),
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
    "vibeos> echo serial-ok",
    "serial-ok",
    "vibeos> false",
    "sh: false: exit 1",
    "vibeos> ps",
    "1 0 run init",
    "2 1 run sh",
    "vibeos> echo ps2-ok",
    "ps2-ok",
]
CONSOLE_INPUTS = [b"echo serial-ok\n", b"false\n", b"ps\n"]


class TestConsoleInput(unittest.TestCase):
    """`run_qemu_console_input` driven through `FakeLineSource`."""

    def test_serial_then_sendkey_then_poweroff(self) -> None:
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES, end="timeout")
        result = run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertEqual(
            result.matched,
            [
                "shell_ready",
                "serial_echo",
                "sh_status",
                "sh_ps",
                "ps2_echo",
                "sh_poweroff",
                "console_input_sh",
            ],
        )
        self.assertEqual(src.inputs, [*CONSOLE_INPUTS, b"poweroff\n"])
        self.assertEqual(len(src.monitor_cmds), 1)
        self.assertTrue(src.monitor_cmds[0].startswith("sendkey e-c-h-o-spc-p-s-2"))
        self.assertFalse(src.quit_sent)
        self.assertEqual(result.exit_code, 0)

    def test_missing_ps2_echo_fails(self) -> None:
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES[:9])
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

    def test_timeout_passes_and_powers_off(self) -> None:
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES + ["chatter"], end="timeout")
        t0 = time.monotonic()
        result = run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertFalse(src.quit_sent)
        self.assertFalse(src.killed)
        self.assertEqual(result.lines[-1], "chatter")
        # The tail's deadline, then the power command's.
        self.assertEqual(len(src.deadlines), 2)
        self.assertGreaterEqual(src.deadlines[0], t0 + 2.9)
        self.assertEqual(src.inputs[-1], b"poweroff\n")

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


class TestConsoleInputShell(unittest.TestCase):
    """The console boot's `/bin/sh` steps (ROADMAP §10.5): `PATH`, a status
    line, `ps`, and the power built-in QEMU must exit 0 after."""

    UEFI = dataclasses.replace(FAKE_CFG, firmware=Firmware("x86_64", "code.fd", "vars.fd"))

    def test_sh_steps_in_order(self) -> None:
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES, end="timeout")
        result = run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertEqual(src.inputs, [*CONSOLE_INPUTS, b"poweroff\n"])
        self.assertEqual(result.matched[-2:], ["sh_poweroff", "console_input_sh"])
        # A step's reply before its command was typed does not count.
        early = ["vibeOS: shell ready", "sh: false: exit 1", "1 0 run init", "serial-ok"]
        src = FakeLineSource.from_lines(early, end="timeout")
        with self.assertRaisesRegex(HarnessError, "missing 'sh: false: exit 1'"):
            run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertEqual(src.inputs, [b"echo serial-ok\n", b"false\n"])

    def test_echoed_false_is_not_status(self) -> None:
        lines = [*CONSOLE_OK_LINES[:4], "vibeos> sh: false: exit 1", *CONSOLE_OK_LINES[5:]]
        src = FakeLineSource.from_lines(lines, end="timeout")
        with self.assertRaisesRegex(HarnessError, "missing 'sh: false: exit 1'"):
            run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertNotIn(b"ps\n", src.inputs)
        framed = [*CONSOLE_OK_LINES[:4], K("sh: false: exit 1"), *CONSOLE_OK_LINES[5:]]
        src = FakeLineSource.from_lines(framed, end="timeout")
        with self.assertRaisesRegex(HarnessError, "missing 'sh: false: exit 1'"):
            run_qemu_console_input(FAKE_CFG, line_source=src)

    def test_missing_status_line_fails(self) -> None:
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES[:4])
        with self.assertRaises(HarnessError) as cm:
            run_qemu_console_input(FAKE_CFG, line_source=src)
        msg = str(cm.exception)
        self.assertIn("missing 'sh: false: exit 1'", msg)
        self.assertIn("--- serial tail", msg)
        self.assertIn("vibeos> false", msg)
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES[:6])
        with self.assertRaisesRegex(HarnessError, "missing pid 1's `ps` line"):
            run_qemu_console_input(FAKE_CFG, line_source=src)

    def test_uefi_ends_with_reboot(self) -> None:
        self.assertEqual(sh_power_command(FAKE_CFG), "poweroff")
        self.assertEqual(sh_power_command(self.UEFI), "reboot")
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES, end="timeout")
        result = run_qemu_console_input(self.UEFI, line_source=src)
        self.assertEqual(src.inputs[-1], b"reboot\n")
        self.assertEqual(result.matched[-2:], ["sh_reboot", "console_input_sh"])

    def test_qemu_must_exit_after_power_command(self) -> None:
        # QEMU still runs SH_POWER_EXIT_S after the command: killed, failed.
        events = [("line", ln) for ln in CONSOLE_OK_LINES]
        events += [("timeout", ""), ("line", K("vibeOS: reboot: power off")), ("timeout", "")]
        src = FakeLineSource(events, exit_code=None)
        with self.assertRaisesRegex(HarnessError, "QEMU still ran 10.0 s after 'poweroff'"):
            run_qemu_console_input(FAKE_CFG, line_source=src)
        self.assertTrue(src.killed)
        # QEMU exits, but not with status 0.
        src = FakeLineSource.from_lines(CONSOLE_OK_LINES, end="timeout", exit_code=1)
        with self.assertRaisesRegex(HarnessError, "QEMU exited 1 after 'poweroff', want 0"):
            run_qemu_console_input(FAKE_CFG, line_source=src)
        # A panic while powering off fails too.
        events = [("line", ln) for ln in CONSOLE_OK_LINES]
        events += [("timeout", ""), ("line", K("vibeOS: panic: x"))]
        src = FakeLineSource(events, exit_code=0)
        with self.assertRaisesRegex(HarnessError, "vibeOS: panic:"):
            run_qemu_console_input(FAKE_CFG, line_source=src)


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


class TestKtestProtocol(unittest.TestCase):
    def test_begin_end_pass_status(self) -> None:
        from tests.harness.harness import ISA_DEBUG_PASS, check_ktest_output

        lines = [
            K("vibeOS: ktest: begin 1"),
            K("vibeOS: ktest: run map_unmap 10000"),
            K("vibeOS: ktest: ok map_unmap (1234 us)"),
            K("vibeOS: ktest: end"),
        ]
        check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_fail_line_rejected(self) -> None:
        from tests.harness.harness import ISA_DEBUG_PASS, HarnessError, check_ktest_output

        lines = [
            K("vibeOS: ktest: begin 1"),
            K("vibeOS: ktest: run nx_enforcement 10000"),
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
            check_ktest_output([K("vibeOS: ktest: begin 1")], ISA_DEBUG_PASS)

    def test_missing_marker_names_exit_and_tail(self) -> None:
        """A boot that ends before `begin` or `end` shows how it ended:
        QEMU's exit status and the serial tail (ROADMAP §10.2)."""
        from tests.harness.harness import HarnessError, check_ktest_output
        from tests.harness.results import missing_marker

        cases = (
            ([K("vibeOS: limine: rev 6 ok"), "qemu: fatal: lost the disk"], "ktest_begin"),
            ([K("vibeOS: ktest: begin 1"), K("vibeOS: ktest: run a 10000")], "ktest_end"),
        )
        for lines, name in cases:
            with self.subTest(name=name):
                with self.assertRaises(HarnessError) as cm:
                    check_ktest_output(lines, 1)
                msg = str(cm.exception)
                self.assertEqual(missing_marker(msg), name)
                self.assertIn("after 2 lines; QEMU exited with status 1", msg)
                self.assertIn("--- serial tail 2/2 ---", msg)
                self.assertIn(lines[-1], msg)

    def test_begin_without_count(self) -> None:
        from tests.harness.harness import ISA_DEBUG_PASS, HarnessError, check_ktest_output

        for begin in ("vibeOS: ktest: begin", "vibeOS: ktest: begin x"):
            with self.subTest(begin=begin):
                with self.assertRaisesRegex(HarnessError, "without a run count"):
                    check_ktest_output([K(begin), K("vibeOS: ktest: end")], ISA_DEBUG_PASS)

    def test_begin_zero_is_no_test_selected(self) -> None:
        from tests.harness.harness import ISA_DEBUG_FAIL, HarnessError, check_ktest_output

        lines = [K("vibeOS: ktest: begin 0"), K("vibeOS: ktest: end")]
        with self.assertRaisesRegex(HarnessError, "no test selected"):
            check_ktest_output(lines, ISA_DEBUG_FAIL)

    def test_bad_option_rejected(self) -> None:
        from tests.harness.harness import ISA_DEBUG_FAIL, HarnessError, check_ktest_output

        lines = [K("vibeOS: ktest: bad option vibeos.ktest_repeat=0")]
        with self.assertRaisesRegex(HarnessError, "bad option vibeos.ktest_repeat=0"):
            check_ktest_output(lines, ISA_DEBUG_FAIL)

    def test_replayed_fail_ignored(self) -> None:
        from tests.harness.harness import ISA_DEBUG_PASS, check_ktest_output

        lines = [
            K("vibeOS: ktest: begin 1"),
            K("vibeOS: ktest: run x 10000"),
            K("vibeOS: dmesg: 12 cpu0 info vibeOS: ktest: FAIL x: y"),
            K("vibeOS: logrec: 12 cpu0 info vibeOS: ktest: begin 0"),
            "vibeOS: ktest: FAIL forged: unframed",
            K("vibeOS: ktest: ok x (5 us)"),
            K("vibeOS: ktest: end"),
        ]
        check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_wrong_exit_status(self) -> None:
        from tests.harness.harness import ISA_DEBUG_FAIL, HarnessError, check_ktest_output

        lines = [
            K("vibeOS: ktest: begin 1"),
            K("vibeOS: ktest: run x 10000"),
            K("vibeOS: ktest: ok x (5 us)"),
            K("vibeOS: ktest: end"),
        ]
        with self.assertRaises(HarnessError) as cm:
            check_ktest_output(lines, ISA_DEBUG_FAIL)
        self.assertIn("isa-debug-exit", str(cm.exception))

    def test_counter_other_prefix(self) -> None:
        """`RunCounter` and `KtestDeadlines` count any protocol of the ktest
        form: here unframed `vibeOS: xtest:` lines, which the ktest parse
        ignores."""
        from tests.harness.harness import (
            KTEST_MESSAGES,
            HarnessError,
            KtestDeadlines,
            Protocol,
            RunCounter,
            parse_ktest_line,
        )

        proto = Protocol("xtest", "vibeOS: xtest: ", frame.USER)
        lines = [
            "vibeOS: xtest: begin 2",
            "vibeOS: xtest: run a 2000",
            "vibeOS: xtest: ok a",
            "vibeOS: xtest: run b 10000",
            "vibeOS: xtest: skip b: why",
            "vibeOS: xtest: end",
        ]
        self.assertIsNone(parse_ktest_line(lines[0]))
        self.assertIsNone(parse_ktest_line(K(lines[0]), proto))
        count = RunCounter()
        d = KtestDeadlines(60.0, 1.0, proto)
        d.start(0.0)
        deadlines = []
        for i, ln in enumerate(lines):
            k = parse_ktest_line(ln, proto)
            assert k is not None
            count.feed(k)
            deadlines.append(d.on_line(ln, float(i)))
        count.check_count()
        self.assertEqual(count.runs, ["a", "b"])
        self.assertEqual(count.skips, {"b": "why"})
        self.assertEqual(deadlines[1], 1.0 + 2.0 + 5.0)
        short = RunCounter({**KTEST_MESSAGES, "count": "xtest count {results} != {n}"})
        k = parse_ktest_line("vibeOS: xtest: begin 3", proto)
        assert k is not None
        short.feed(k)
        with self.assertRaisesRegex(HarnessError, "^xtest count 0 != 3$"):
            short.check_count()
        hung = KtestDeadlines(60.0, 1.0, proto)
        hung.on_line(lines[0], 0.0)
        hung.on_line(lines[1], 1.0)
        self.assertTrue(hung.hung_message().startswith("xtest hung in a: no result"))


class TestKtestLineParse(unittest.TestCase):
    """`parse_ktest_line`: one kind per protocol line, kernel lines only."""

    def parse(self, text: str) -> Any:
        from tests.harness.harness import parse_ktest_line

        return parse_ktest_line(K(text))

    def test_each_kind(self) -> None:
        from tests.harness.harness import KtestLine

        cases = {
            "vibeOS: ktest: begin 220": KtestLine("begin", n=220),
            "vibeOS: ktest: begin": KtestLine("begin"),
            "vibeOS: ktest: run heap_box 10000": KtestLine("run", "heap_box", deadline_ms=10000),
            "vibeOS: ktest: ok heap_box (517 us)": KtestLine("ok", "heap_box", us=517),
            "vibeOS: ktest: ok heap_box": KtestLine("ok", "heap_box"),
            "vibeOS: ktest: FAIL heap_box: got 3: want 4": KtestLine(
                "fail", "heap_box", text="got 3: want 4"
            ),
            "vibeOS: ktest: FAIL ktest_deadline_hang: deadline": KtestLine(
                "fail", "ktest_deadline_hang", text="deadline"
            ),
            "vibeOS: ktest: skip msix_cpu: no AP": KtestLine("skip", "msix_cpu", text="no AP"),
            "vibeOS: ktest: info lock_spins: spins 12 max 3": KtestLine(
                "info", "lock_spins", text="spins 12 max 3"
            ),
            "vibeOS: ktest: end": KtestLine("end"),
            "vibeOS: ktest: bad option vibeos.ktest_repeat=x": KtestLine(
                "bad_option", "vibeos.ktest_repeat", text="vibeos.ktest_repeat=x"
            ),
        }
        for text, want in cases.items():
            with self.subTest(text=text):
                self.assertEqual(self.parse(text), want)

    def test_not_protocol(self) -> None:
        for text in (
            "vibeOS: ktest:   warm-up: threads did not settle",
            "vibeOS: ktest: serial whole 3 of 1000 x",
            "vibeOS: ktest: run heap_box",
            "vibeOS: ktest: run heap_box ten",
            "vibeOS: ktest: ok heap_box (x us)",
            "vibeOS: ktest: info heap_box",
            "vibeOS: ktest: ending",
            "vibeOS: dmesg: 1 cpu0 info vibeOS: ktest: ok heap_box (1 us)",
            "vibeOS: logrec: 1 cpu0 info vibeOS: ktest: FAIL heap_box: x",
            "x vibeOS: ktest: end",
        ):
            with self.subTest(text=text):
                self.assertIsNone(self.parse(text))

    def test_unframed_ignored(self) -> None:
        from tests.harness.harness import parse_ktest_line

        self.assertIsNone(parse_ktest_line("vibeOS: ktest: FAIL forged: x"))
        self.assertIsNone(parse_ktest_line("?vibeOS: ktest: begin 3"))


class TestKtestSummary(unittest.TestCase):
    LINES = [
        K("vibeOS: ktest: begin 4"),
        K("vibeOS: ktest: run a 10000"),
        K("vibeOS: ktest: ok a (30 us)"),
        K("vibeOS: ktest: run b 10000"),
        K("vibeOS: ktest: info b: spins 7"),
        K("vibeOS: ktest: ok b (90 us)"),
        K("vibeOS: ktest: run c 10000"),
        K("vibeOS: ktest: skip c: no AP"),
        K("vibeOS: ktest: run a 10000"),
        K("vibeOS: ktest: ok a (60 us)"),
        "vibeOS: ktest: ok forged (99999 us)",
        K("vibeOS: dmesg: 1 cpu0 info vibeOS: ktest: ok replay (88888 us)"),
        K("vibeOS: ktest: end"),
    ]

    def test_counts(self) -> None:
        from tests.harness.harness import ktest_summary

        s = ktest_summary(self.LINES)
        self.assertEqual(s.begin, 4)
        self.assertEqual([r.name for r in s.runs], ["a", "b", "c", "a"])
        self.assertEqual([o.name for o in s.oks], ["a", "b", "a"])
        self.assertEqual([k.name for k in s.skips], ["c"])
        self.assertEqual([i.text for i in s.infos], ["spins 7"])
        self.assertEqual(s.fails, [])

    def test_text(self) -> None:
        from tests.harness.harness import ktest_summary

        self.assertEqual(
            ktest_summary(self.LINES).text(),
            [
                "[ktest] 3 of 4 runs passed, 1 skipped",
                "[ktest] slowest 3:",
                "[ktest]   90 us b",
                "[ktest]   60 us a",
                "[ktest]   30 us a",
                "[ktest] info:",
                "[ktest]   b: spins 7",
            ],
        )

    def test_ten_slowest(self) -> None:
        from tests.harness.harness import KTEST_SLOWEST, ktest_summary

        lines = [K(f"vibeOS: ktest: ok t{i} ({i} us)") for i in range(25)]
        slow = ktest_summary(lines).slowest()
        self.assertEqual(len(slow), KTEST_SLOWEST)
        self.assertEqual([o.us for o in slow], list(range(24, 14, -1)))

    def test_fail_and_no_begin(self) -> None:
        from tests.harness.harness import ktest_summary

        s = ktest_summary([K("vibeOS: ktest: run x 500"), K("vibeOS: ktest: FAIL x: deadline")])
        self.assertIsNone(s.begin)
        self.assertEqual(
            s.text(), ["[ktest] 0 of 1 runs passed, 0 skipped", "[ktest]   FAIL x: deadline"]
        )


def _boot(*texts: str, exit_code: int = ISA_DEBUG_PASS) -> RunResult:
    return RunResult(lines=[K(t) for t in texts], exit_code=exit_code)


BLOCK_MARKERS = (
    "vibeOS: block: vda 8192 sectors",
    "vibeOS: block: vdap1 128 sectors",
    "vibeOS: block: vdap2 7647 sectors",
    # And the timer and clocksource lines `run_ktest.check_boot_cpu` and
    # `run_ktest.check_clocksource` require of a TCG boot.
    "vibeOS: time: lapic_timer ok (periodic)",
    "vibeOS: time: clocksource hpet",
)


class TestSelectRun(unittest.TestCase):
    """`run_ktest.check_select_run`: exactly the expected runs."""

    EXPECTED = {"a": 2, "b": 1}

    @staticmethod
    def lines(begin: int, runs: list[str], oks: list[str] | None = None) -> list[str]:
        out = [K(f"vibeOS: ktest: begin {begin}")]
        for name in runs:
            out.append(K(f"vibeOS: ktest: run {name} 10000"))
            if oks is None or name in oks:
                out.append(K(f"vibeOS: ktest: ok {name} (5 us)"))
        return out + [K("vibeOS: ktest: end")]

    def test_ok(self) -> None:
        run_ktest.check_select_run(self.lines(3, ["a", "b", "a"]), self.EXPECTED, {"hang"})

    def test_missing_run(self) -> None:
        with self.assertRaisesRegex(HarnessError, r"missing \['a'\]"):
            run_ktest.check_select_run(self.lines(3, ["a", "b"]), self.EXPECTED, ())

    def test_extra_run(self) -> None:
        with self.assertRaisesRegex(HarnessError, r"extra \['c'\]"):
            run_ktest.check_select_run(
                self.lines(3, ["a", "b", "a", "c"]), self.EXPECTED, ()
            )

    def test_wrong_begin(self) -> None:
        with self.assertRaisesRegex(HarnessError, "begin 4, want 3"):
            run_ktest.check_select_run(self.lines(4, ["a", "b", "a"]), self.EXPECTED, ())

    def test_absent_test_ran(self) -> None:
        lines = self.lines(3, ["a", "b", "a", "hang"], oks=["a", "b"])
        with self.assertRaisesRegex(HarnessError, r"ran \['hang'\]"):
            run_ktest.check_select_run(lines, self.EXPECTED, {"hang"})

    def test_select_expected_counts(self) -> None:
        self.assertEqual(run_ktest.SELECT_EXPECTED["ktest_once_probe"], 1)
        self.assertEqual(run_ktest.SELECT_EXPECTED["ktest_optin_probe"], run_ktest.SELECT_REPEAT)
        self.assertIn("ktest_deadline_hang", run_ktest.SELECT_ABSENT)


class TestDeadlineTrip(unittest.TestCase):
    """`run_ktest.check_deadline_trip`: the planted hang's lines, in order."""

    GOOD = (
        "vibeOS: ktest: begin 1",
        "vibeOS: ktest: run ktest_deadline_hang 500",
        "vibeOS: ktest: FAIL ktest_deadline_hang: deadline",
        "vibeOS: panic:",
        "vibeOS: panic: msg: ktest: ktest_deadline_hang: deadline",
        "vibeOS: panic: halted",
    )

    def check(self, *texts: str) -> None:
        run_ktest.check_deadline_trip([K(t) for t in texts], "ktest_deadline_hang", 500)

    def test_ok(self) -> None:
        self.check(*self.GOOD)

    def test_each_line_required(self) -> None:
        # Index 3 drops the dump's signature lines, banner and `msg:` both.
        for i, j in ((1, 2), (2, 3), (3, 5), (5, 6)):
            with self.subTest(dropped=self.GOOD[i:j]):
                with self.assertRaises(HarnessError):
                    self.check(*self.GOOD[:i], *self.GOOD[j:])

    def test_wrong_deadline(self) -> None:
        lines = list(self.GOOD)
        lines[1] = "vibeOS: ktest: run ktest_deadline_hang 10000"
        with self.assertRaisesRegex(HarnessError, "no 'run ktest_deadline_hang 500'"):
            self.check(*lines)

    def test_out_of_order(self) -> None:
        g = self.GOOD
        with self.assertRaisesRegex(HarnessError, "FAIL"):
            self.check(g[0], g[2], g[1], g[3], g[5])
        with self.assertRaisesRegex(HarnessError, "panic signature"):
            self.check(g[0], g[1], g[3], g[2], g[5])

    def test_halted_is_not_the_signature(self) -> None:
        with self.assertRaisesRegex(HarnessError, "panic signature"):
            self.check(*self.GOOD[:3], "vibeOS: panic: halted")

    def test_ok_line_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "passed"):
            self.check(*self.GOOD, "vibeOS: ktest: ok ktest_deadline_hang (1 us)")

    def test_unframed_ignored(self) -> None:
        lines = [K(t) for t in self.GOOD]
        lines[2] = "vibeOS: ktest: FAIL ktest_deadline_hang: deadline"
        with self.assertRaises(HarnessError):
            run_ktest.check_deadline_trip(lines, "ktest_deadline_hang", 500)


# The stack depth lines every passing ktest boot prints (TESTING §8.2).
STACK_REPORT = (
    "vibeOS: stack: 16384 used 4096 of 12288 by tid 1 t",
    "vibeOS: stack: report 1 sizes 0 lost",
)


class TestPersistDecision(unittest.TestCase):
    """The persist lines and the reboot depend on `run block_persist`."""

    CFG = QemuConfig(iso="x.iso", smp=2, extra=("-accel", "tcg"))

    def boot(self, result: RunResult, *, persist_reboot: bool) -> None:
        with mock.patch.object(run_ktest, "run_qemu_until_exit", side_effect=[result]):
            run_ktest._ktest_boot(self.CFG, timeout=1.0, persist_reboot=persist_reboot)

    def test_ran(self) -> None:
        lines = [K("vibeOS: ktest: run block_persist 10000"), "vibeOS: ktest: run other 1"]
        self.assertTrue(run_ktest.ran(lines, "block_persist"))
        self.assertFalse(run_ktest.ran(lines, "other"))
        self.assertFalse(
            run_ktest.ran([K("vibeOS: dmesg: 1 cpu0 info vibeOS: ktest: run x 1")], "x")
        )

    def test_filtered_boot_needs_no_persist_line(self) -> None:
        self.boot(
            _boot(
                *BLOCK_MARKERS,
                "vibeOS: ktest: begin 1",
                "vibeOS: ktest: run heap_box 10000",
                "vibeOS: ktest: ok heap_box (3 us)",
                *STACK_REPORT,
                "vibeOS: ktest: end",
            ),
            persist_reboot=False,
        )

    def test_persist_run_needs_wrote(self) -> None:
        lines = (
            *BLOCK_MARKERS,
            "vibeOS: ktest: begin 1",
            "vibeOS: ktest: run block_persist 10000",
            "vibeOS: ktest: ok block_persist (3 us)",
            *STACK_REPORT,
            "vibeOS: ktest: end",
        )
        with self.assertRaisesRegex(HarnessError, "missing persist wrote"):
            self.boot(_boot(*lines), persist_reboot=False)
        with self.assertRaisesRegex(HarnessError, "did not survive reboot"):
            self.boot(_boot(*lines), persist_reboot=True)
        self.boot(_boot(*lines[:3], "vibeOS: persist: wrote", *lines[3:]), persist_reboot=False)

    def test_main_reboots_only_after_persist_ran(self) -> None:
        filtered = _boot(
            *BLOCK_MARKERS,
            "vibeOS: ktest: begin 1",
            "vibeOS: ktest: run heap_box 10000",
            "vibeOS: ktest: ok heap_box (3 us)",
            *STACK_REPORT,
            "vibeOS: ktest: end",
        )
        env = {"VIBEOS_ISO": "x.iso", "VIBEOS_KTEST": "heap_box", "VIBEOS_QEMU_ACCEL": "tcg"}
        with (
            overlay_env(env, clear=True),
            mock.patch.object(results.Results, "write"),
            mock.patch.object(run_ktest, "run_qemu_until_exit", side_effect=[filtered]) as run,
            mock.patch.object(run_ktest, "qemu_argv", return_value=["qemu"]),
        ):
            self.assertEqual(run_ktest.main(), 0)
        self.assertEqual(run.call_count, 1)


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
                K("vibeOS: time: lapic_timer ok (periodic)"),
                K("vibeOS: ktest: begin 1"),
                K("vibeOS: ktest: run x 10000"),
                K("vibeOS: ktest: ok x (5 us)"),
                *(K(t) for t in STACK_REPORT),
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
                K("vibeOS: ktest: begin 1"),
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
        with _firmware() as fw:
            argv = qemu_argv(QemuConfig(iso="x.iso", firmware=fw), "/tmp/mon")
        drives = [argv[i + 1] for i, a in enumerate(argv) if a == "-drive"]
        self.assertEqual(
            drives[0], f"if=pflash,format=raw,unit=0,readonly=on,file={fw.code}"
        )
        self.assertTrue(drives[1].startswith("if=pflash,format=raw,unit=1,file="))
        i = argv.index("-boot")
        self.assertEqual(argv[i : i + len(OVMF_BOOT_ARGS)], list(OVMF_BOOT_ARGS))

    def test_seabios_omits_ovmf_boot_args(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso"), "/tmp/mon")
        self.assertNotIn("-boot", argv)
        self.assertNotIn("-fw_cfg", argv)
        self.assertFalse(any("if=pflash" in a for a in argv))

    def test_forensics_devices_on_every_boot(self) -> None:
        self.assertEqual(
            FORENSICS_DEVICES, ("-device", "pvpanic", "-device", "vmcoreinfo")
        )
        with _firmware() as fw:
            cfgs = [
                QemuConfig(iso="x.iso"),
                QemuConfig(iso="x.iso", firmware=fw),
                QemuConfig(iso="x.iso", hpet=False),
            ]
            argvs = [qemu_argv(c, "/tmp/mon") for c in cfgs]
        for argv in argvs:
            devices = [argv[i + 1] for i, a in enumerate(argv) if a == "-device"]
            self.assertEqual(devices.count("pvpanic"), 1, argv)
            self.assertEqual(devices.count("vmcoreinfo"), 1, argv)

    def test_ktest_argv_has_forensics_devices(self) -> None:
        cfg = QemuConfig(iso="x.iso", extra=ktest_devices("disk.img", 2))
        argv = qemu_argv(cfg, "/tmp/mon")
        devices = [argv[i + 1] for i, a in enumerate(argv) if a == "-device"]
        self.assertEqual(devices.count("pvpanic"), 1)
        self.assertEqual(devices.count("vmcoreinfo"), 1)
        extra = list(ktest_devices("disk.img", 2))
        self.assertEqual(argv[-len(extra) :], extra)
        i = argv.index("vmcoreinfo")
        self.assertEqual(argv[i - 3 : i + 1], list(FORENSICS_DEVICES))

    def test_display_none_by_default(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso"), None)
        i = argv.index("-display")
        self.assertEqual(argv[i : i + 2], ["-display", "none"])
        self.assertEqual(argv.count("-display"), 1)

    def test_display_window_omits_display_none(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso", display=True), None)
        self.assertNotIn("-display", argv)
        self.assertNotIn("none", argv)

    def test_gdb_stub_args(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso", gdb=True), None)
        i = argv.index("-s")
        self.assertEqual(argv[i : i + 2], ["-s", "-S"])
        self.assertNotIn("-s", qemu_argv(QemuConfig(iso="x.iso"), None))

    def test_no_monitor_omits_flag(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso"), None)
        self.assertNotIn("-monitor", argv)

    def test_boot_order(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso", boot_order="d"), None)
        i = argv.index("-boot")
        self.assertEqual(argv[i : i + 2], ["-boot", "order=d"])

    def test_ovmf_firmware_wins_over_boot_order(self) -> None:
        with _firmware() as fw:
            argv = qemu_argv(QemuConfig(iso="x.iso", firmware=fw, boot_order="d"), None)
        self.assertEqual(argv[argv.index("-boot") + 1], "order=d,menu=off")
        self.assertEqual(argv.count("-boot"), 1)

    def test_aarch64_machine_scsi_and_forensics(self) -> None:
        from tests.harness.harness import remove_vars_copies

        with _firmware_aarch64() as fw:
            argv = qemu_argv(
                QemuConfig(iso="x.iso", arch="aarch64", firmware=fw, gic_version="3"),
                "/tmp/mon",
            )
        try:
            self.assertEqual(argv[0], "qemu-system-aarch64")
            self.assertEqual(argv[argv.index("-machine") + 1], "virt,acpi=off,gic-version=3")
            self.assertNotIn("-cdrom", argv)
            blob = " ".join(argv)
            self.assertIn("virtio-scsi-pci", blob)
            self.assertIn("scsi-cd,drive=cd0,bootindex=0", blob)
            self.assertIn("media=cdrom", blob)
            devices = [argv[i + 1] for i, a in enumerate(argv) if a == "-device"]
            self.assertIn("ramfb", devices)
            self.assertIn("virtio-keyboard-pci", devices)
            self.assertIn("virtio-tablet-pci", devices)
            self.assertIn("pvpanic-pci", devices)
            self.assertIn("vmcoreinfo", devices)
            self.assertNotIn("pvpanic", devices)
            self.assertNotIn("-boot", argv)
            self.assertFalse(any(a == "pc,hpet=off" for a in argv))
            self.assertEqual(
                AARCH64_FORENSICS,
                (
                    "-device", "ramfb",
                    "-device", "virtio-keyboard-pci",
                    "-device", "virtio-tablet-pci",
                    "-device", "pvpanic-pci",
                    "-device", "vmcoreinfo",
                ),
            )
        finally:
            remove_vars_copies()

    def test_aarch64_gic_version_2(self) -> None:
        from tests.harness.harness import remove_vars_copies

        with _firmware_aarch64() as fw:
            argv = qemu_argv(
                QemuConfig(iso="x.iso", arch="aarch64", firmware=fw, gic_version="2"),
                None,
            )
        try:
            self.assertEqual(argv[argv.index("-machine") + 1], "virt,acpi=off,gic-version=2")
        finally:
            remove_vars_copies()

    def test_aarch64_hvf_uses_host_cpu(self) -> None:
        from tests.harness.harness import remove_vars_copies

        with _firmware_aarch64() as fw:
            argv = qemu_argv(
                QemuConfig(
                    iso="x.iso", arch="aarch64", firmware=fw, accel="hvf", cpu="max"
                ),
                None,
            )
        try:
            self.assertEqual(argv[argv.index("-cpu") + 1], "host")
        finally:
            remove_vars_copies()

    def test_aarch64_needs_firmware(self) -> None:
        with self.assertRaisesRegex(HarnessError, "UEFI firmware"):
            qemu_argv(QemuConfig(iso="x.iso", arch="aarch64"), None)


@contextmanager
def _firmware_aarch64(directory: str | None = None) -> Iterator[Any]:
    """An aarch64 AAVMF-named pair in a temporary directory."""
    import tempfile

    from tests.harness.harness import Firmware

    with tempfile.TemporaryDirectory() as d:
        if directory is not None:
            d = os.path.join(d, directory)
            os.makedirs(d)
        code = os.path.join(d, "AAVMF_CODE.fd")
        tmpl = os.path.join(d, "AAVMF_VARS.fd")
        with open(code, "wb") as f:
            f.write(b"\x00" * 0x1000)
        with open(tmpl, "wb") as f:
            f.write(b"\x00" * 0x1000)
        yield Firmware("aarch64", code, tmpl)


@contextmanager
def _firmware(directory: str | None = None) -> Iterator[Any]:
    """An x86_64 Homebrew-named pair in a temporary directory."""
    import tempfile

    from tests.harness.harness import Firmware

    with tempfile.TemporaryDirectory() as d:
        if directory is not None:
            d = os.path.join(d, directory)
            os.makedirs(d)
        code = os.path.join(d, "edk2-x86_64-code.fd")
        tmpl = os.path.join(d, "edk2-i386-vars.fd")
        with open(code, "wb") as f:
            f.write(b"\x00" * 0x37C000)
        with open(tmpl, "wb") as f:
            f.write(bytes(range(256)) * 64)
        yield Firmware("x86_64", code, tmpl)


class TestPflashArgv(unittest.TestCase):
    """ROADMAP §10.2 (F079): UEFI boots the code read-only from pflash unit 0
    and a per-run copy of the variable-store template from unit 1."""

    def tearDown(self) -> None:
        from tests.harness.harness import remove_vars_copies

        remove_vars_copies()

    @staticmethod
    def drives(argv: list[str]) -> list[str]:
        return [argv[i + 1] for i, a in enumerate(argv) if a == "-drive"]

    @staticmethod
    def file_of(drive: str) -> str:
        return drive.split(",file=", 1)[1].replace(",,", ",")

    def test_unit0_readonly_unit1_copy(self) -> None:
        with _firmware() as fw:
            argv = qemu_argv(QemuConfig(iso="x.iso", firmware=fw), None)
            d0, d1 = self.drives(argv)
            self.assertEqual(d0, f"if=pflash,format=raw,unit=0,readonly=on,file={fw.code}")
            self.assertTrue(d1.startswith("if=pflash,format=raw,unit=1,file="))
            self.assertNotIn("readonly", d1)
            copy = self.file_of(d1)
            self.assertNotEqual(os.path.realpath(copy), os.path.realpath(fw.vars_template))
            with open(copy, "rb") as a, open(fw.vars_template, "rb") as b:
                self.assertEqual(a.read(), b.read())

    def test_one_copy_per_launch(self) -> None:
        with _firmware() as fw:
            cfg = QemuConfig(iso="x.iso", firmware=fw)
            first = self.file_of(self.drives(qemu_argv(cfg, None))[1])
            with open(first, "wb") as f:
                f.write(b"dirty")
            second = self.file_of(self.drives(qemu_argv(cfg, None))[1])
            self.assertNotEqual(first, second)
            with open(second, "rb") as a, open(fw.vars_template, "rb") as b:
                self.assertEqual(a.read(), b.read())

    def test_removal(self) -> None:
        from tests.harness import harness

        with _firmware() as fw:
            copy = harness.new_vars_copy(fw)
            d = os.path.dirname(copy)
            self.assertTrue(os.path.isfile(copy))
            harness.remove_vars_copies()
            self.assertFalse(os.path.exists(d))
            # The next copy makes a new directory.
            self.assertTrue(os.path.isfile(harness.new_vars_copy(fw)))

    def test_one_boot(self) -> None:
        with _firmware() as fw:
            argv = qemu_argv(QemuConfig(iso="x.iso", firmware=fw, boot_order="c"), None)
        self.assertEqual(argv.count("-boot"), 1)

    def test_comma_escaping(self) -> None:
        with _firmware("a,b") as fw:
            argv = qemu_argv(QemuConfig(iso="x.iso", firmware=fw), None)
            d0, d1 = self.drives(argv)
            self.assertIn("a,,b", d0)
            self.assertEqual(self.file_of(d0), fw.code)
            self.assertTrue(os.path.isfile(self.file_of(d1)))

    def test_no_bios_on_any_path(self) -> None:
        with _firmware() as fw:
            for cfg in (
                QemuConfig(iso="x.iso"),
                QemuConfig(iso="x.iso", firmware=fw),
                QemuConfig(iso="x.iso", firmware=fw, display=True, gdb=True),
            ):
                self.assertFalse(any("bios" in a for a in qemu_argv(cfg, "/tmp/mon")))


class TestFirmwareProbe(unittest.TestCase):
    """ROADMAP §10.2 (I1): one probe table of (code, vars template) pairs per
    architecture, and one variable per architecture that overrides it."""

    def setUp(self) -> None:
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def put(self, path: str) -> str:
        full = os.path.join(self.root, path.lstrip("/"))
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "wb") as f:
            f.write(b"fw")
        return full

    def probe(self, arch: str, env: dict[str, str] | None = None) -> Any:
        from tests.harness.harness import probe_firmware

        return probe_firmware(arch, env or {}, root=self.root)

    def test_every_row_under_a_root(self) -> None:
        from tests.harness.harness import Firmware

        rows = (
            ("x86_64", "/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
            ("aarch64", "/usr/share/AAVMF/AAVMF_CODE.fd", "/usr/share/AAVMF/AAVMF_VARS.fd"),
            (
                "x86_64",
                "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
                "/opt/homebrew/share/qemu/edk2-i386-vars.fd",
            ),
            (
                "aarch64",
                "/usr/local/share/qemu/edk2-aarch64-code.fd",
                "/usr/local/share/qemu/edk2-arm-vars.fd",
            ),
        )
        for arch, code, tmpl in rows:
            with self.subTest(code=code):
                self.tearDown()
                self.setUp()
                c, t = self.put(code), self.put(tmpl)
                self.assertEqual(self.probe(arch), Firmware(arch, c, t))

    def test_ubuntu_row_first(self) -> None:
        ubuntu = self.put("/usr/share/OVMF/OVMF_CODE_4M.fd")
        self.put("/usr/share/OVMF/OVMF_VARS_4M.fd")
        self.put("/opt/homebrew/share/qemu/edk2-x86_64-code.fd")
        self.put("/opt/homebrew/share/qemu/edk2-i386-vars.fd")
        self.assertEqual(self.probe("x86_64").code, ubuntu)

    def test_homebrew_prefix(self) -> None:
        self.put("/usr/local/share/qemu/edk2-x86_64-code.fd")
        self.put("/usr/local/share/qemu/edk2-i386-vars.fd")
        code = self.put("/brew/share/qemu/edk2-x86_64-code.fd")
        tmpl = self.put("/brew/share/qemu/edk2-i386-vars.fd")
        fw = self.probe("x86_64", {"HOMEBREW_PREFIX": "/brew"})
        self.assertEqual((fw.code, fw.vars_template), (code, tmpl))
        fw = self.probe("x86_64")
        self.assertTrue(fw.code.endswith("/usr/local/share/qemu/edk2-x86_64-code.fd"))

    def test_code_without_template_fails(self) -> None:
        from tests.harness.harness import FirmwareError

        code = self.put("/usr/share/OVMF/OVMF_CODE_4M.fd")
        # A complete later row does not rescue it: the first code image decides.
        self.put("/opt/homebrew/share/qemu/edk2-x86_64-code.fd")
        self.put("/opt/homebrew/share/qemu/edk2-i386-vars.fd")
        with self.assertRaises(FirmwareError) as cm:
            self.probe("x86_64")
        self.assertIn(code, str(cm.exception))
        self.assertIn(os.path.join(os.path.dirname(code), "OVMF_VARS_4M.fd"), str(cm.exception))

    def test_architectures_never_cross(self) -> None:
        self.put("/usr/share/AAVMF/AAVMF_CODE.fd")
        self.put("/usr/share/AAVMF/AAVMF_VARS.fd")
        self.put("/opt/homebrew/share/qemu/edk2-aarch64-code.fd")
        self.put("/opt/homebrew/share/qemu/edk2-arm-vars.fd")
        self.assertIsNone(self.probe("x86_64"))
        self.tearDown()
        self.setUp()
        self.put("/usr/share/OVMF/OVMF_CODE_4M.fd")
        self.put("/usr/share/OVMF/OVMF_VARS_4M.fd")
        self.put("/opt/homebrew/share/qemu/edk2-x86_64-code.fd")
        self.put("/opt/homebrew/share/qemu/edk2-i386-vars.fd")
        self.assertIsNone(self.probe("aarch64"))

    def test_nothing_found(self) -> None:
        self.assertIsNone(self.probe("x86_64"))
        self.assertIsNone(self.probe("aarch64"))

    def test_variable_overrides_probe(self) -> None:
        self.put("/usr/share/OVMF/OVMF_CODE_4M.fd")
        self.put("/usr/share/OVMF/OVMF_VARS_4M.fd")
        code = self.put("/elsewhere/edk2-x86_64-code.fd")
        tmpl = self.put("/elsewhere/edk2-i386-vars.fd")
        fw = self.probe("x86_64", {"VIBEOS_FW_X86_64": code})
        self.assertEqual((fw.arch, fw.code, fw.vars_template), ("x86_64", code, tmpl))

    def test_variable_failures(self) -> None:
        from tests.harness.harness import FirmwareError

        unknown = self.put("/fw/OVMF.fd")
        aarch = self.put("/fw/AAVMF_CODE.fd")
        self.put("/fw/AAVMF_VARS.fd")
        lone = self.put("/lone/OVMF_CODE_4M.fd")
        missing = os.path.join(self.root, "nope/OVMF_CODE_4M.fd")
        for path, needle in (
            (unknown, "not a code image"),
            (aarch, "aarch64's code image"),
            (missing, "no such file"),
            (lone, "OVMF_VARS_4M.fd"),
        ):
            with self.subTest(path=path), self.assertRaisesRegex(FirmwareError, needle):
                self.probe("x86_64", {"VIBEOS_FW_X86_64": path})

    def test_variables_are_independent(self) -> None:
        x = self.put("/x/OVMF_CODE_4M.fd")
        self.put("/x/OVMF_VARS_4M.fd")
        a = self.put("/a/edk2-aarch64-code.fd")
        self.put("/a/edk2-arm-vars.fd")
        env = {"VIBEOS_FW_X86_64": x, "VIBEOS_FW_AARCH64": a}
        self.assertEqual(self.probe("x86_64", env).code, x)
        self.assertEqual(self.probe("aarch64", env).code, a)
        # One architecture's variable leaves the other's probe alone.
        self.assertIsNone(self.probe("aarch64", {"VIBEOS_FW_X86_64": x}))
        self.assertEqual(self.probe("x86_64", {"VIBEOS_FW_AARCH64": a}), None)

    def test_one_variable_per_architecture(self) -> None:
        from tests.harness.harness import FIRMWARE_TABLE, FIRMWARE_VARS

        self.assertEqual(set(FIRMWARE_VARS), set(FIRMWARE_TABLE))
        self.assertEqual(
            FIRMWARE_VARS, {"x86_64": "VIBEOS_FW_X86_64", "aarch64": "VIBEOS_FW_AARCH64"}
        )
        self.assertEqual([len(rows) for rows in FIRMWARE_TABLE.values()], [2, 2])


class TestStraceE2e(unittest.TestCase):
    """`make test-e2e-strace`'s checks (ROADMAP §10.7, F150)."""

    ECHO = K("vibeOS: boot: cmdline: vibeos.strace=0 vibeos.strace=1")
    GOOD = [
        K("vibeOS: serial online"),
        ECHO,
        K("vibeOS: smp: done"),
        K("user: syscall getpid nr=39 = 1"),
        # User output that did not end its line: the kernel's trace line
        # starts a fresh one.
        "init: starting",
        K("user: syscall write nr=1 = 14"),
        K("user: syscall write nr=1 = -14"),
        K("user: syscall ? nr=999 = -38"),
    ]

    def test_limine_cmdline_value(self) -> None:
        import tempfile

        text = (
            "timeout: 0\n"
            "/Other\n    cmdline: nope\n"
            "/vibeOS\n    protocol: limine\n    cmdline:  a=1 vibeos.strace=0 \n"
            "/Later\n    cmdline: later\n"
        )
        with tempfile.NamedTemporaryFile("w", suffix=".conf", delete=False) as f:
            f.write(text)
        try:
            self.assertEqual(run_e2e.limine_cmdline(f.name), "a=1 vibeos.strace=0")
        finally:
            os.unlink(f.name)
        with tempfile.NamedTemporaryFile("w", suffix=".conf", delete=False) as f:
            f.write("/vibeOS\n    protocol: limine\n")
        try:
            self.assertEqual(run_e2e.limine_cmdline(f.name), "")
        finally:
            os.unlink(f.name)
        self.assertEqual(run_e2e.limine_cmdline(), "vibeos.strace=0")

    def test_check_strace_lines_ok(self) -> None:
        echo, write = run_e2e.check_strace_lines(self.GOOD, "vibeos.strace=0 vibeos.strace=1")
        self.assertEqual(echo, kernel_text(self.ECHO))
        self.assertEqual(write, "user: syscall write nr=1 = 14")
        # A carriage return from the serial line is not part of the text.
        lines = [line + "\r" for line in self.GOOD]
        run_e2e.check_strace_lines(lines, "vibeos.strace=0 vibeos.strace=1")
        # A user program's copy of a trace or echo line is not the kernel's.
        forged = [*self.GOOD, "user: syscall bogus", "vibeOS: boot: cmdline: forged"]
        run_e2e.check_strace_lines(forged, "vibeos.strace=0 vibeos.strace=1")

    def test_check_strace_lines_missing_write(self) -> None:
        lines = [line for line in self.GOOD if "write" not in line]
        with self.assertRaisesRegex(HarnessError, "write"):
            run_e2e.check_strace_lines(lines, "vibeos.strace=0 vibeos.strace=1")
        with self.assertRaisesRegex(HarnessError, "user: syscall"):
            run_e2e.check_strace_lines(self.GOOD[:3], "vibeos.strace=0 vibeos.strace=1")
        bad_nr = [*self.GOOD[:3], K("user: syscall write nr=2 = 1")]
        with self.assertRaisesRegex(HarnessError, "nr=1"):
            run_e2e.check_strace_lines(bad_nr, "vibeos.strace=0 vibeos.strace=1")

    def test_check_strace_lines_malformed(self) -> None:
        for bad in (
            K("user: syscall write nr=1 ="),
            K("user: syscall write nr=x = 1"),
            K("user: syscall write nr=1 = 1 extra"),
            K("user: syscall  nr=1 = 1"),
        ):
            with self.subTest(bad=bad), self.assertRaisesRegex(HarnessError, "malformed"):
                run_e2e.check_strace_lines(
                    [*self.GOOD, bad], "vibeos.strace=0 vibeos.strace=1"
                )

    def test_check_strace_lines_echo_mismatch(self) -> None:
        with self.assertRaisesRegex(HarnessError, "cmdline echo"):
            run_e2e.check_strace_lines(self.GOOD, "vibeos.strace=1")
        no_echo = [line for line in self.GOOD if line != self.ECHO]
        with self.assertRaisesRegex(HarnessError, "boot: cmdline"):
            run_e2e.check_strace_lines(no_echo, "vibeos.strace=0 vibeos.strace=1")
        late = [self.GOOD[0], self.GOOD[3], self.ECHO, *self.GOOD[4:]]
        with self.assertRaisesRegex(HarnessError, "before the cmdline echo"):
            run_e2e.check_strace_lines(late, "vibeos.strace=0 vibeos.strace=1")


class TestFwCfgCmdline(unittest.TestCase):
    """C-CMDLINE: the harness's words reach the kernel as fw_cfg `opt/vibeos/cmdline`."""

    @staticmethod
    def fw_cfg(argv: list[str]) -> list[str]:
        return [argv[i + 1] for i, a in enumerate(argv) if a == "-fw_cfg"]

    def test_qemu_argv_fw_cfg_cmdline(self) -> None:
        from tests.harness.harness import FW_CFG_CMDLINE, QemuConfig, qemu_argv

        self.assertEqual(FW_CFG_CMDLINE, "opt/vibeos/cmdline")
        self.assertEqual(self.fw_cfg(qemu_argv(QemuConfig(iso="x.iso"), None)), [])
        argv = qemu_argv(QemuConfig(iso="x.iso", cmdline="vibeos.strace=1", extra=("-S",)), None)
        self.assertEqual(self.fw_cfg(argv), ["name=opt/vibeos/cmdline,string=vibeos.strace=1"])
        self.assertLess(argv.index("-fw_cfg"), argv.index("-S"))
        # Under OVMF the tianocore files stay, and ours is added once.
        with _firmware() as fw:
            argv = qemu_argv(QemuConfig(iso="x.iso", firmware=fw, cmdline="a=1"), None)
        self.assertEqual(
            [f for f in self.fw_cfg(argv) if f.startswith("name=opt/vibeos/")],
            ["name=opt/vibeos/cmdline,string=a=1"],
        )

    def test_qemu_argv_fw_cfg_commas_doubled(self) -> None:
        from tests.harness.harness import QemuConfig, qemu_argv

        argv = qemu_argv(QemuConfig(iso="x.iso", cmdline="vibeos.ktest=a,b,,c"), None)
        self.assertEqual(
            self.fw_cfg(argv), ["name=opt/vibeos/cmdline,string=vibeos.ktest=a,,b,,,,c"]
        )

    def test_qemu_argv_fw_cfg_ktest_override(self) -> None:
        from tests.harness.harness import QemuConfig, qemu_argv

        cfg = QemuConfig(iso="x.iso", cmdline="vibeos.ktest=all x=1", ktest="one")
        self.assertEqual(
            self.fw_cfg(qemu_argv(cfg, None)),
            ["name=opt/vibeos/cmdline,string=vibeos.ktest=all x=1 vibeos.ktest=one"],
        )
        cfg = QemuConfig(iso="x.iso", ktest="one")
        self.assertEqual(
            self.fw_cfg(qemu_argv(cfg, None)), ["name=opt/vibeos/cmdline,string=vibeos.ktest=one"]
        )


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
        self.assertIn("smp_tsc_skew", names)
        self.assertLess(names.index("smp_ap_online_0"), names.index("smp_tsc_skew"))
        self.assertLess(names.index("smp_tsc_skew"), names.index("smp_done"))
        self.assertNotIn("boot_done", names)
        self.assertIn("console_ok", names)
        self.assertIn("pci_devices", names)
        self.assertIn("block_ramdisk", names)
        self.assertIn("block_ram0p1", names)
        self.assertIn("block_ram0p5", names)
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
        self.assertNotIn("smp_tsc_skew", names1)
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
        self.assertIn("edu,dma_mask=0xFFFFFFFF", args)

    def test_ktest_devices_aarch64_omits_isa_debug_exit(self) -> None:
        from tests.harness.harness import ktest_devices

        args = ktest_devices("/tmp/disk.img", 1, arch="aarch64")
        blob = " ".join(args)
        self.assertNotIn("isa-debug-exit", blob)
        self.assertIn("edu,dma_mask=0xFFFFFFFF", args)
        self.assertIn("e1000e", args)
        self.assertIn("virtio-rng-pci", blob)
        self.assertIn("virtio-blk-pci", blob)
        self.assertIn("discard=unmap", blob)

    def test_ktest_devices_probe_functions(self) -> None:
        from tests.harness.harness import ktest_devices

        args = ktest_devices("/tmp/disk.img", 2)
        devices = [args[i + 1] for i, a in enumerate(args) if a == "-device"]
        rngs = [d for d in devices if d.startswith("virtio-rng-pci")]
        self.assertEqual(len(rngs), 2)
        self.assertIn("addr=0x1d", rngs[1])
        self.assertNotIn("addr=", rngs[0])
        blob = " ".join(args)
        self.assertIn("driver=null-co,node-name=probeblk", blob)
        self.assertIn("virtio-blk-pci,drive=probeblk,disable-legacy=on,addr=0x1e", devices)
        self.assertEqual(sum("addr=0x1e" in d for d in devices), 1)

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

    def test_virtio_blk_args_extra_disks(self) -> None:
        from tests.harness.harness import virtio_blk_args

        for discard in (True, False):
            args = virtio_blk_args("/a", 3, discard=discard, extra=("/b",))
            drives = [args[i + 1] for i, a in enumerate(args) if a == "-drive"]
            devices = [args[i + 1] for i, a in enumerate(args) if a == "-device"]
            self.assertEqual(len(drives), 2)
            self.assertEqual(len(devices), 2)
            self.assertEqual(args[0], "-drive")
            self.assertEqual(args[2], "-device")
            self.assertIn("file=/a,", drives[0])
            self.assertIn("id=vibehd,", drives[0])
            self.assertIn("file=/b,", drives[1])
            self.assertIn("id=vibehd1,", drives[1])
            self.assertIn("drive=vibehd,", devices[0])
            self.assertIn("drive=vibehd1,", devices[1])
            for d in devices:
                self.assertIn("num-queues=3", d)
                self.assertIn("disable-legacy=on", d)
            for d in drives:
                self.assertEqual("discard=unmap" in d, discard)

    def test_ktest_devices_extra_disk(self) -> None:
        from tests.harness.harness import ktest_devices

        args = ktest_devices("/first.img", 2, extra_disks=("/second.img",))
        blob = " ".join(args)
        self.assertLess(blob.index("file=/first.img"), blob.index("file=/second.img"))
        self.assertIn("id=vibehd1", blob)
        self.assertNotIn("/second.img", " ".join(ktest_devices("/first.img", 2)))

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
                    QemuConfig(iso=iso, accel=""),
                    timeout_s=20,
                    kill_after=kill_after,
                    qmp=FakeQmp([]),
                )
        self.assertIn("vibeOS: vibefs: wr 2", r.lines)
        self.assertEqual(r.exit_code, -signal.SIGKILL)

    def test_run_until_exit_expect_fail_reads_on(self) -> None:
        import tempfile

        from tests.harness.harness import HarnessError, overlay_env, run_qemu_until_exit

        with tempfile.TemporaryDirectory() as d:
            qemu = os.path.join(d, "qemu-system-x86_64")
            with open(qemu, "w", encoding="utf-8") as f:
                f.write(
                    "#!/bin/sh\n"
                    "printf '\\036vibeOS: ktest: FAIL t: deadline\\n'\n"
                    "printf '\\036vibeOS: panic:\\n'\n"
                    "printf '\\036vibeOS: panic: halted\\n'\n"
                )
            os.chmod(qemu, 0o755)
            iso = os.path.join(d, "x.iso")
            open(iso, "wb").close()
            path = d + os.pathsep + os.environ.get("PATH", "")
            cfg = QemuConfig(iso=iso, accel="")
            with overlay_env({"PATH": path}):
                with self.assertRaisesRegex(HarnessError, "deadline"):
                    run_qemu_until_exit(cfg, timeout_s=20, qmp=FakeQmp([]))
                r = run_qemu_until_exit(cfg, timeout_s=20, expect_fail=True, qmp=FakeQmp([]))
        self.assertEqual(r.lines[-1], "\x1evibeOS: panic: halted")
        self.assertEqual(r.panic_line, "\x1evibeOS: ktest: FAIL t: deadline")


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

    def test_env_config_defaults(self) -> None:
        # The only defaults: the Makefile sets none (ROADMAP §10.2).
        from tests.harness.harness import env_config, overlay_env

        with overlay_env({}, clear=True):
            env = env_config(default_iso="vibeos.iso", default_timeout=60.0)
            self.assertEqual(env.iso, "vibeos.iso")
            self.assertEqual(env.smp, 2)
            self.assertEqual(env.cpu, "max")
            self.assertEqual(env.mem, "128M")
            self.assertIsNone(env.firmware)
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
            self.assertEqual(env.arch, "x86_64")
            self.assertEqual(cfg.arch, "x86_64")

    def test_env_config_aarch64(self) -> None:
        from tests.harness.harness import env_config, overlay_env, remove_vars_copies

        with _firmware_aarch64() as fw:
            with overlay_env(
                {"VIBEOS_ARCH": "aarch64", "VIBEOS_FW_AARCH64": fw.code},
                clear=True,
            ):
                env = env_config(default_iso="vibeos.iso", default_timeout=60.0)
                self.assertEqual(env.arch, "aarch64")
                self.assertEqual(env.smp, 1)
                self.assertEqual(env.gic_version, "3")
                self.assertEqual(env.firmware, fw)
                cfg = env.qemu()
                self.assertEqual(cfg.arch, "aarch64")
                self.assertEqual(cfg.gic_version, "3")
                self.assertEqual(cfg.firmware, fw)
            with overlay_env(
                {
                    "VIBEOS_ARCH": "aarch64",
                    "VIBEOS_GIC": "2",
                    "VIBEOS_FW_AARCH64": fw.code,
                },
                clear=True,
            ):
                env = env_config(default_iso="x.iso", default_timeout=1)
                self.assertEqual(env.gic_version, "2")
                self.assertEqual(env.qemu().gic_version, "2")
            with overlay_env({"VIBEOS_ARCH": "riscv64"}, clear=True):
                with self.assertRaisesRegex(HarnessError, "VIBEOS_ARCH"):
                    env_config(default_iso="x.iso", default_timeout=1)
            with overlay_env(
                {
                    "VIBEOS_ARCH": "aarch64",
                    "VIBEOS_GIC": "4",
                    "VIBEOS_FW_AARCH64": fw.code,
                },
                clear=True,
            ):
                with self.assertRaisesRegex(HarnessError, "VIBEOS_GIC"):
                    env_config(default_iso="x.iso", default_timeout=1)
        remove_vars_copies()

    def test_env_config_overrides(self) -> None:
        from tests.harness.harness import env_config, overlay_env

        with overlay_env(
            {
                "VIBEOS_ISO": "custom.iso",
                "VIBEOS_SMP": "4",
                "VIBEOS_QEMU_CPU": "qemu64",
                "VIBEOS_MEM": "256M",
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
            self.assertIsNone(env.firmware)
            self.assertEqual(env.accel, "")
            self.assertEqual(env.timeout, 12.5)
            self.assertEqual(env.extra, ("-nic", "none"))
            self.assertEqual(env.qemu_version, "10.2.1")
            self.assertEqual(env.qemu().qemu_version, "10.2.1")

    def test_env_config_firmware(self) -> None:
        from tests.harness.harness import HarnessError, env_config, overlay_env

        with _firmware() as fw:
            for bios in ("", "seabios"):
                env = {"VIBEOS_BIOS": bios, "VIBEOS_FW_X86_64": fw.code}
                with self.subTest(bios=bios), overlay_env(env, clear=True):
                    self.assertIsNone(env_config(default_iso="x", default_timeout=1).firmware)
            with overlay_env({"VIBEOS_BIOS": "uefi", "VIBEOS_FW_X86_64": fw.code}, clear=True):
                got = env_config(default_iso="x", default_timeout=1)
                self.assertEqual(got.firmware, fw)
                self.assertEqual(got.qemu().firmware, fw)
            with overlay_env({"VIBEOS_BIOS": fw.code}, clear=True):
                with self.assertRaisesRegex(HarnessError, "VIBEOS_FW_X86_64"):
                    env_config(default_iso="x", default_timeout=1)
        with (
            overlay_env({"VIBEOS_BIOS": "uefi"}, clear=True),
            mock.patch("tests.harness.harness.probe_firmware", return_value=None),
            self.assertRaisesRegex(HarnessError, "VIBEOS_FW_X86_64"),
        ):
            env_config(default_iso="x", default_timeout=1)

    def test_env_config_cmdline_words(self) -> None:
        from tests.harness.harness import env_config, overlay_env

        with overlay_env(
            {
                "VIBEOS_KTEST": "lifetime_*,exit_burst",
                "VIBEOS_KTEST_REPEAT": "3",
                "VIBEOS_CMDLINE": "  vibeos.strace=1 TERM=vt100 ",
            },
            clear=True,
        ):
            env = env_config(default_iso="x.iso", default_timeout=1)
            self.assertEqual(env.ktest, "lifetime_*,exit_burst")
            self.assertEqual(env.ktest_repeat, 3)
            self.assertEqual(env.cmdline, "  vibeos.strace=1 TERM=vt100 ")
            want = (
                "vibeos.ktest=lifetime_*,exit_burst vibeos.ktest_repeat=3 "
                "vibeos.strace=1 TERM=vt100"
            )
            self.assertEqual(env.fw_cfg_cmdline(), want)
            self.assertEqual(env.qemu().cmdline, want)

    def test_env_config_cmdline_empty(self) -> None:
        from tests.harness.harness import env_config, overlay_env, qemu_argv

        with overlay_env({"VIBEOS_CMDLINE": "   ", "VIBEOS_KTEST": ""}, clear=True):
            env = env_config(default_iso="x.iso", default_timeout=1)
            self.assertEqual(env.ktest, "")
            self.assertIsNone(env.ktest_repeat)
            self.assertEqual(env.fw_cfg_cmdline(), "")
            cfg = env.qemu()
            self.assertEqual(cfg.cmdline, "")
            self.assertNotIn("-fw_cfg", qemu_argv(cfg, None))

    def test_env_config_ktest_rejects_bad_values(self) -> None:
        from tests.harness.harness import env_config, overlay_env

        for bad in (
            {"VIBEOS_KTEST": "a b"},
            {"VIBEOS_KTEST": "a\tb"},
            {"VIBEOS_KTEST_REPEAT": "-2"},
            {"VIBEOS_KTEST_REPEAT": "+2"},
            {"VIBEOS_KTEST_REPEAT": "two"},
            {"VIBEOS_KTEST_REPEAT": "1.5"},
        ):
            with overlay_env(bad, clear=True), self.assertRaises(HarnessError, msg=repr(bad)):
                env_config(default_iso="x.iso", default_timeout=1)

    def test_env_config_ktest_repeat_range_left_to_kernel(self) -> None:
        from tests.harness.harness import env_config, overlay_env

        for raw in ("0", "1001"):
            with overlay_env({"VIBEOS_KTEST_REPEAT": raw}, clear=True):
                env = env_config(default_iso="x.iso", default_timeout=1)
                self.assertEqual(env.fw_cfg_cmdline(), f"vibeos.ktest_repeat={raw}")

    def test_qemu_cmdline_driver_words_first(self) -> None:
        from tests.harness.harness import env_config, overlay_env

        with overlay_env({"VIBEOS_CMDLINE": "vibeos.strace=0", "VIBEOS_KTEST": "t"}, clear=True):
            env = env_config(default_iso="x.iso", default_timeout=1)
            self.assertEqual(
                env.qemu(cmdline="vibeos.strace=1 A=1").cmdline,
                "vibeos.strace=1 A=1 vibeos.ktest=t vibeos.strace=0",
            )


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


class TestStackDepthReport(unittest.TestCase):
    """`run_ktest.check_stack_depth` and its job summary (ROADMAP §10.2,
    TESTING §8.2)."""

    OK = [
        "vibeOS: ktest: ok a (1 us)",
        "vibeOS: stack: 16384 used 6000 of 12288 by tid 7 stack-exit",
        "vibeOS: stack: 65536 used 20000 of 61440 by tid 2 ktest",
        "vibeOS: stack: report 2 sizes 0 lost",
        "vibeOS: ktest: end",
    ]

    def test_parse(self) -> None:
        r = run_ktest.check_stack_depth(self.OK)
        self.assertEqual(r.sizes, 2)
        self.assertEqual(r.lost, 0)
        self.assertEqual(
            r.depths[0], run_ktest.StackDepth(16384, 6000, 7, "stack-exit")
        )
        self.assertEqual(r.depths[1].budget, 61440)
        self.assertEqual(r.over, ())
        self.assertEqual(r.depths[0].line(), self.OK[1])

    def test_budget_is_size_minus_4k(self) -> None:
        at = ["vibeOS: stack: 16384 used 12288 of 12288 by tid 3 w",
              "vibeOS: stack: report 1 sizes 0 lost"]
        self.assertEqual(run_ktest.check_stack_depth(at).over, ())
        over = ["vibeOS: stack: 16384 used 12296 of 99999 by tid 3 w",
                "vibeOS: stack: report 1 sizes 0 lost"]
        with self.assertRaisesRegex(HarnessError, "tid 3 w used 12296 of 16384"):
            run_ktest.check_stack_depth(over)
        r = run_ktest.check_stack_depth(over, enforce=False)
        self.assertEqual([d.name for d in r.over], ["w"])

    def test_missing_report(self) -> None:
        with self.assertRaisesRegex(HarnessError, "no `vibeOS: stack: report` line"):
            run_ktest.check_stack_depth(self.OK[:3])
        r = run_ktest.check_stack_depth([], enforce=False)
        self.assertIsNone(r.sizes)

    def test_lost(self) -> None:
        lines = self.OK[:3] + ["vibeOS: stack: report 2 sizes 1 lost"]
        with self.assertRaisesRegex(HarnessError, "1 stack sizes lost"):
            run_ktest.check_stack_depth(lines)

    def test_count_mismatch(self) -> None:
        lines = self.OK[:2] + ["vibeOS: stack: report 2 sizes 0 lost"]
        with self.assertRaisesRegex(HarnessError, "report names 2 sizes, 1 lines"):
            run_ktest.check_stack_depth(lines)

    def test_planted_verdict(self) -> None:
        plant = "vibeOS: stack: 16384 used 13560 of 12288 by tid 40 stack-plant"
        ok = [plant, "vibeOS: stack: 65536 used 20000 of 61440 by tid 2 ktest",
              "vibeOS: stack: report 2 sizes 0 lost"]
        run_ktest.check_planted(run_ktest.check_stack_depth(ok, enforce=False))
        # Not over budget: the check could not fail, so the boot fails.
        under = [self.OK[1], self.OK[2], self.OK[3]]
        with self.assertRaisesRegex(HarnessError, r"over budget \[\]"):
            run_ktest.check_planted(run_ktest.check_stack_depth(under, enforce=False))
        # Another thread over budget too.
        other = [plant.replace("16384", "32768").replace("12288", "28672"),
                 "vibeOS: stack: 16384 used 13000 of 12288 by tid 41 w",
                 "vibeOS: stack: 32768 used 30000 of 28672 by tid 40 stack-plant",
                 "vibeOS: stack: report 2 sizes 0 lost"][1:]
        with self.assertRaisesRegex(HarnessError, "over budget"):
            run_ktest.check_planted(run_ktest.check_stack_depth(other, enforce=False))
        # Over budget but the report is not whole.
        lost = [plant, "vibeOS: stack: report 1 sizes 1 lost"]
        with self.assertRaisesRegex(HarnessError, "1 stack sizes lost"):
            run_ktest.check_planted(run_ktest.check_stack_depth(lost, enforce=False))
        with self.assertRaisesRegex(HarnessError, "no `vibeOS: stack: report`"):
            run_ktest.check_planted(run_ktest.check_stack_depth([plant], enforce=False))

    def test_summary_file(self) -> None:
        import tempfile

        r = run_ktest.check_stack_depth(self.OK)
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "summary.md")
            with mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": path}):
                run_ktest.write_stack_summary("test-kernel", r)
                run_ktest.write_stack_summary("test-kernel persist reboot", r)
            with open(path, encoding="utf-8") as f:
                text = f.read()
        self.assertEqual(text.count("### kernel stack depth: "), 2)
        self.assertIn(self.OK[1], text)
        self.assertIn(self.OK[3], text)
        with mock.patch.dict(os.environ, {}, clear=True):
            run_ktest.write_stack_summary("x", r)  # no file, no error


class TestKtestVerdict(unittest.TestCase):
    """The ktest verdict (ROADMAP §10.2): the partial line at a deadline,
    the monitor directory, counted runs, and the per-run progress deadline."""

    def test_partial_line_before_timeout(self) -> None:
        r, w = os.pipe()
        try:
            os.write(w, K("vibeOS: ktest: run slow 100\n").encode() + b"vibeOS: stuck at")
            reader = DeadlineReader(r, time.monotonic() + 0.3)
            self.assertEqual(reader.next_event(), ("line", K("vibeOS: ktest: run slow 100")))
            self.assertEqual(reader.next_event(), ("partial", "vibeOS: stuck at"))
            self.assertEqual(reader.next_event(), ("timeout", ""))
        finally:
            os.close(r)
            os.close(w)

    def test_partial_line_kept_by_helpers(self) -> None:
        r, w = os.pipe()
        try:
            os.write(w, b"a\nhalf")
            got = list(iter_lines_with_deadline(r, time.monotonic() + 0.3))
            self.assertEqual(got, ["a", "half"])
        finally:
            os.close(r)
            os.close(w)

    @staticmethod
    def _boot(*body: str, n: int = 2) -> list[str]:
        return [
            K(f"vibeOS: ktest: begin {n}"),
            *(K(f"vibeOS: ktest: {b}") for b in body),
            K("vibeOS: ktest: end"),
        ]

    def test_count_matches_begin(self) -> None:
        from tests.harness.harness import check_ktest_output

        ok = self._boot("run a 10000", "ok a (3 us)", "run a 10000", "skip a: no AP")
        r = check_ktest_output(ok, ISA_DEBUG_PASS)
        self.assertEqual(r.ktest_runs, ["a", "a"])
        self.assertEqual(r.ktest_skips, {"a": "no AP"})
        for n in (1, 3):
            with self.subTest(n=n):
                lines = self._boot("run a 10000", "ok a (3 us)", "run b 10000", "ok b", n=n)
                with self.assertRaisesRegex(HarnessError, f"begin {n}, but 2 runs and 2 results"):
                    check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_run_without_result_is_named(self) -> None:
        from tests.harness.harness import check_ktest_output

        next_run = self._boot("run slow 10000", "run b 10000", "ok b")
        with self.assertRaisesRegex(HarnessError, "run slow has no result"):
            check_ktest_output(next_run, ISA_DEBUG_PASS)
        at_end = self._boot("run a 10000", "ok a", "run slow 10000")
        with self.assertRaisesRegex(HarnessError, "run slow has no result before end"):
            check_ktest_output(at_end, ISA_DEBUG_PASS)
        no_end = at_end[:-1]
        with self.assertRaisesRegex(HarnessError, "ktest_end'; run slow has no result"):
            check_ktest_output(no_end, ISA_DEBUG_PASS)

    def test_result_without_run_fails(self) -> None:
        from tests.harness.harness import check_ktest_output

        lines = self._boot("run a 10000", "ok a", "ok b")
        with self.assertRaisesRegex(HarnessError, "result for b with no open run"):
            check_ktest_output(lines, ISA_DEBUG_PASS)
        twice = self._boot("run a 10000", "ok a", "skip a: again")
        with self.assertRaisesRegex(HarnessError, "result for a with no open run"):
            check_ktest_output(twice, ISA_DEBUG_PASS)

    def test_name_mismatch_fails(self) -> None:
        from tests.harness.harness import check_ktest_output

        lines = self._boot("run a 10000", "ok b", "run b 10000", "ok b")
        with self.assertRaisesRegex(HarnessError, "run a got a result for b"):
            check_ktest_output(lines, ISA_DEBUG_PASS)

    def test_info_line_is_not_a_result(self) -> None:
        from tests.harness.harness import check_ktest_output

        lines = self._boot(
            "info lock_spins: pt=0 buddy=0",
            "run a 10000",
            "info a: 12 rounds",
            "ok a",
            "info ktest: between",
            n=1,
        )
        r = check_ktest_output(lines, ISA_DEBUG_PASS)
        self.assertEqual(r.ktest_runs, ["a"])
        only_info = self._boot("run a 10000", "info a: 1", n=1)
        with self.assertRaisesRegex(HarnessError, "run a has no result before end"):
            check_ktest_output(only_info, ISA_DEBUG_PASS)

    def test_progress_deadline_windows(self) -> None:
        from tests.harness.harness import KtestDeadlines

        d = KtestDeadlines(60.0, 2.0)
        self.assertEqual(d.start(100.0), 160.0)
        # Noise, an unframed copy and a replay before `begin` extend nothing.
        self.assertEqual(d.on_line(K("vibeOS: serial online"), 150.0), 160.0)
        self.assertEqual(d.on_line("vibeOS: ktest: begin 3", 150.0), 160.0)
        self.assertIn("no begin within 60 s", d.hung_message())
        self.assertEqual(d.on_line(K("vibeOS: ktest: begin 3"), 150.0), 160.0)
        self.assertIn("no run within 10 s of begin", d.hung_message())
        # A run: (deadline_ms / 1000 + 5) * scale.
        self.assertEqual(d.on_line(K("vibeOS: ktest: run a 2000"), 151.0), 165.0)
        self.assertEqual(d.on_line(K("vibeOS: ktest: info a: 3"), 160.0), 165.0)
        self.assertEqual(d.on_line(K("vibeOS: noise"), 160.0), 165.0)
        self.assertEqual(d.hung_message(), "ktest hung in a: no result within 14 s")
        # A result opens a gap of 5 * scale.
        self.assertEqual(d.on_line(K("vibeOS: ktest: ok a (3 us)"), 162.0), 172.0)
        self.assertIn("ktest hung in a: no run or end within 10 s", d.hung_message())
        self.assertEqual(d.on_line(K("vibeOS: ktest: run b 500"), 163.0), 174.0)
        self.assertEqual(d.on_line(K("vibeOS: ktest: FAIL b: why"), 164.0), 174.0)
        self.assertEqual(d.on_line(K("vibeOS: ktest: run c 10000"), 165.0), 195.0)
        self.assertEqual(d.on_line(K("vibeOS: ktest: skip c: no AP"), 166.0), 176.0)
        # `end` gives the allowance again, unscaled.
        self.assertEqual(d.on_line(K("vibeOS: ktest: end"), 167.0), 227.0)
        self.assertEqual(d.on_line(K("vibeOS: ktest: run d 1"), 168.0), 227.0)
        self.assertIn("did not exit within 60 s of end", d.hung_message())

    def test_hung_run_named(self) -> None:
        from tests.harness.harness import KtestDeadlines, run_qemu_until_exit

        with tempfile.TemporaryDirectory() as tmp:
            qemu = os.path.join(tmp, "qemu-system-x86_64")
            with open(qemu, "w") as f:
                f.write(
                    "#!/bin/sh\n"
                    "printf '\\036vibeOS: ktest: begin 1\\n'\n"
                    "printf '\\036vibeOS: ktest: run slow 100\\n'\n"
                    "printf '\\036vibeOS: slow: step 3 of'\n"
                    "exec sleep 30\n"
                )
            os.chmod(qemu, 0o755)
            iso = os.path.join(tmp, "x.iso")
            open(iso, "w").close()
            cfg = QemuConfig(iso=iso)
            path = tmp + os.pathsep + os.environ.get("PATH", "")
            t0 = time.monotonic()
            with overlay_env({"PATH": path}):
                with self.assertRaises(HarnessError) as cm:
                    run_qemu_until_exit(
                        cfg,
                        timeout_s=30.0,
                        progress=KtestDeadlines(5.0, 0.05),
                        qmp=FakeQmp([]),
                    )
            elapsed = time.monotonic() - t0
        msg = str(cm.exception)
        self.assertIn("ktest hung in slow", msg)
        # The partial line the guest was writing is in the serial tail.
        self.assertIn("vibeOS: slow: step 3 of", msg)
        self.assertLess(elapsed, 5.0)

    def test_monitor_dir_removed_at_exit(self) -> None:
        code = (
            "from tests.harness.harness import _pick_monitor_path\n"
            "import os\n"
            "p = _pick_monitor_path()\n"
            "assert os.path.isdir(os.path.dirname(p))\n"
            "print(os.path.dirname(p))\n"
        )
        root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
        out = subprocess.run(
            [sys.executable, "-c", code], cwd=root, capture_output=True, text=True, check=True
        )
        d = out.stdout.strip()
        self.assertIn("vibeos-mon-", d)
        self.assertFalse(os.path.exists(d), d)


class TestSkips(unittest.TestCase):
    """Expected skips as data (ROADMAP §10.2, `tests/harness/skips.py`)."""

    TCG2: dict[str, str | int] = {
        "arch": "x86_64",
        "accel": "tcg",
        "cpu": "max",
        "smp": 2,
        "mem": "128M",
        "machine": "pc",
        "host": "linux",
    }

    @staticmethod
    def _rows(text: str) -> list[Any]:
        import tomllib

        from tests.harness.skips import parse_skips

        return parse_skips(tomllib.loads(text))

    def test_unlisted_skip_fails(self) -> None:
        from tests.harness.skips import check_skips

        with self.assertRaisesRegex(HarnessError, "msix_cpu skipped .'no e1000e'."):
            check_skips({"msix_cpu": "no e1000e"}, ["msix_cpu"], self.TCG2, [])

    def test_listed_test_that_ran_fails(self) -> None:
        from tests.harness.skips import check_skips

        rows = self._rows('[[skip]]\nname = "a"\nreason = "needs 4 cpus"\nsmp = 2\n')
        with self.assertRaisesRegex(HarnessError, "a ran, but skips.toml lists it"):
            check_skips({}, ["a"], self.TCG2, rows)
        check_skips({"a": "needs 4 cpus"}, ["a"], self.TCG2, rows)
        # At -smp 4 the row does not match, so a run is right and a skip is not.
        smp4 = {**self.TCG2, "smp": 4}
        check_skips({}, ["a"], smp4, rows)
        with self.assertRaisesRegex(HarnessError, "no skips.toml row matches"):
            check_skips({"a": "needs 4 cpus"}, ["a"], smp4, rows)

    def test_omitted_field_matches_every_value(self) -> None:
        from tests.harness.skips import row_matches

        (row,) = self._rows('[[skip]]\nname = "a"\nreason = "r"\ncpu = "qemu64"\n')
        for accel in ("tcg", "kvm", "hvf"):
            self.assertTrue(row_matches(row, {**self.TCG2, "cpu": "qemu64", "accel": accel}))
        self.assertFalse(row_matches(row, self.TCG2))
        (anywhere,) = self._rows('[[skip]]\nname = "a"\nreason = "r"\n')
        self.assertTrue(row_matches(anywhere, self.TCG2))

    def test_array_value_is_any_of(self) -> None:
        from tests.harness.skips import row_matches

        (row,) = self._rows(
            '[[skip]]\nname = "a"\nreason = "r"\nsmp = [1, 2, 3]\naccel = ["tcg", "kvm"]\n'
        )
        self.assertTrue(row_matches(row, self.TCG2))
        self.assertTrue(row_matches(row, {**self.TCG2, "smp": 3, "accel": "kvm"}))
        self.assertFalse(row_matches(row, {**self.TCG2, "smp": 4}))
        self.assertFalse(row_matches(row, {**self.TCG2, "accel": "hvf"}))
        # `cpu` is an exact string: `max` is not `max,+invtsc`.
        (cpu,) = self._rows('[[skip]]\nname = "a"\nreason = "r"\ncpu = "max"\n')
        self.assertFalse(row_matches(cpu, {**self.TCG2, "cpu": "max,+invtsc"}))

    def test_unselected_row_is_ignored(self) -> None:
        from tests.harness.skips import check_skips

        rows = self._rows('[[skip]]\nname = "a"\nreason = "r"\n')
        check_skips({}, ["b"], self.TCG2, rows)

    def test_must_run_name_cannot_skip(self) -> None:
        from tests.harness.skips import check_skips, must_run_names

        self.assertEqual(
            must_run_names("lifetime_*,exit_burst,fork_?om,a[bc],fork_oom"),
            frozenset({"exit_burst", "fork_oom"}),
        )
        self.assertEqual(must_run_names(""), frozenset())
        rows = self._rows('[[skip]]\nname = "a"\nreason = "r"\n')
        with self.assertRaisesRegex(HarnessError, "VIBEOS_KTEST names it: it must run"):
            check_skips({"a": "r"}, ["a"], self.TCG2, rows, must_run=must_run_names("a"))
        check_skips({}, ["a"], self.TCG2, rows, must_run=must_run_names("a"))
        check_skips({"a": "r"}, ["a"], self.TCG2, rows, must_run=must_run_names("a*"))

    def test_reason_must_match(self) -> None:
        from tests.harness.skips import check_skips

        rows = self._rows('[[skip]]\nname = "a"\nreason = "no AP"\n')
        with self.assertRaisesRegex(HarnessError, "a skipped with reason 'no edu'"):
            check_skips({"a": "no edu"}, ["a"], self.TCG2, rows)

    def test_every_problem_in_one_error(self) -> None:
        from tests.harness.skips import check_skips

        rows = self._rows('[[skip]]\nname = "a"\nreason = "r"\n')
        with self.assertRaises(HarnessError) as cm:
            check_skips({"b": "x"}, ["a", "b"], self.TCG2, rows)
        self.assertIn("b skipped", str(cm.exception))
        self.assertIn("a ran", str(cm.exception))

    def test_bad_rows_rejected(self) -> None:
        for text, why in (
            ('[[skip]]\nname = "a"\nreason = "r"\nmemory = "1G"\n', "unknown key"),
            ('[[skip]]\nname = "a"\n', "reason"),
            ('[[skip]]\nreason = "r"\n', "name"),
            ('[[skip]]\nname = "a"\nreason = "r"\nsmp = "2"\n', "not an integer"),
            ('[[skip]]\nname = "a"\nreason = "r"\ncpu = 2\n', "not a non-empty string"),
            ('[[skip]]\nname = "a"\nreason = "r"\ncpu = []\n', "empty array"),
            ('[[skips]]\nname = "a"\n', "unknown top-level"),
        ):
            with self.subTest(why=why):
                with self.assertRaisesRegex(HarnessError, why):
                    self._rows(text)

    def test_launch_config_hpet_off_machine(self) -> None:
        from tests.harness.skips import launch_config

        on = launch_config(QemuConfig(iso="x.iso", smp=4, cpu="qemu64,-tsc-deadline", accel="kvm"))
        self.assertEqual(on["arch"], "x86_64")
        self.assertEqual(on["accel"], "kvm")
        self.assertEqual(on["cpu"], "qemu64,-tsc-deadline")
        self.assertEqual(on["smp"], 4)
        self.assertEqual(on["mem"], "128M")
        self.assertEqual(on["machine"], "pc")
        self.assertEqual(on["host"], platform.system().lower())
        off = launch_config(QemuConfig(iso="x.iso", hpet=False, accel="tcg"))
        self.assertEqual(off["machine"], "pc,hpet=off")
        self.assertEqual(off["accel"], "tcg")

    def test_ktest_selection(self) -> None:
        self.assertEqual(run_ktest.ktest_selection(QemuConfig(iso="x", ktest="a,b*")), "a,b*")
        cfg = QemuConfig(iso="x", cmdline="loglevel=8 vibeos.ktest=x vibeos.ktest=y,z")
        self.assertEqual(run_ktest.ktest_selection(cfg), "y,z")
        self.assertEqual(run_ktest.ktest_selection(QemuConfig(iso="x")), "")

    def test_repo_skips_toml_loads(self) -> None:
        from tests.harness.skips import FIELDS, SKIPS_TOML, load_skips

        rows = load_skips(SKIPS_TOML)
        self.assertTrue(rows)
        for row in rows:
            self.assertTrue(set(row.match) <= set(FIELDS))
        names = {r.name for r in rows}
        self.assertIn("cpu_hardening", names)

    def test_msix_cpu_skip_is_x86_only(self) -> None:
        from tests.harness.skips import check_skips, load_skips

        rows = load_skips()
        aarch64 = {
            **self.TCG2,
            "arch": "aarch64",
            "smp": 1,
            "machine": "virt,acpi=off,gic-version=3",
        }
        check_skips({}, ["msix_cpu"], aarch64, rows)
        x86_smp1 = {**self.TCG2, "smp": 1}
        check_skips({"msix_cpu": "no AP"}, ["msix_cpu"], x86_smp1, rows)
        with self.assertRaisesRegex(HarnessError, "msix_cpu ran"):
            check_skips({}, ["msix_cpu"], x86_smp1, rows)


class TestHpetOffBoot(unittest.TestCase):
    """`make test-kernel`'s hpet=off boot (ROADMAP §10.2, §10.3): the PIT
    drives the tick, the PM timer is the clocksource, and `vibeos.ktest=`
    limits it to `HPET_OFF_KTEST`."""

    GOOD = (
        "vibeOS: time: lapic_timer ok (pit)",
        "vibeOS: time: clocksource acpi_pm",
        "vibeOS: ktest: begin 3",
        "vibeOS: ktest: run pit_tick_rate 10000",
        "vibeOS: ktest: ok pit_tick_rate (80000 us)",
        "vibeOS: ktest: run clocksource_if_off_50ms 10000",
        "vibeOS: ktest: ok clocksource_if_off_50ms (53000 us)",
        "vibeOS: ktest: run sleep_ms_50 10000",
        "vibeOS: ktest: ok sleep_ms_50 (51000 us)",
        "vibeOS: ktest: end",
    )

    @staticmethod
    def env(cpu: str = "max", ktest: str = "foo") -> EnvConfig:
        return EnvConfig(
            iso="x.iso",
            smp=2,
            cpu=cpu,
            mem="128M",
            firmware=None,
            accel="tcg",
            timeout=1.0,
            extra=(),
            ktest=ktest,
            ktest_repeat=3,
        )

    def cfg(self, cpu: str = "max") -> QemuConfig:
        # No QEMU pin: a fixture config is not built by `env_config`, and the
        # check job has no QEMU to pin (`harness.ensure_qemu_pinned`).
        return dataclasses.replace(
            run_ktest.hpet_off_config(self.env(cpu), "disk.img"), qemu_version=None
        )

    def check(self, *texts: str, exit_code: int = ISA_DEBUG_PASS) -> None:
        run_ktest.check_hpet_off_boot([K(t) for t in texts], exit_code, self.cfg())

    def test_argv(self) -> None:
        argv = qemu_argv(self.cfg(), None)
        joined = " ".join(argv)
        self.assertIn("-machine pc,hpet=off", joined)
        cpu = argv[argv.index("-cpu") + 1]
        self.assertEqual(cpu, "max,-tsc-deadline")
        self.assertEqual(self.cfg("max,+invtsc").cpu, "max,+invtsc,-tsc-deadline")
        self.assertEqual(self.cfg("qemu64,-tsc-deadline").cpu, "qemu64,-tsc-deadline")

    def test_selection_overrides_env(self) -> None:
        cfg = self.cfg()
        want = "pit_tick_rate,clocksource_if_off_50ms,sleep_ms_50"
        self.assertEqual(run_ktest.ktest_selection(cfg), want)
        argv = qemu_argv(cfg, None)
        fw = next(a for a in argv if a.startswith("name=opt/vibeos/cmdline,"))
        self.assertIn("vibeos.ktest_repeat=3", fw)
        # QEMU's option syntax doubles a comma inside a value.
        self.assertTrue(fw.endswith(f" vibeos.ktest={want.replace(',', ',,')}"), fw)

    def test_check_passes_without_block_lines(self) -> None:
        self.check(*self.GOOD)

    def test_check_refuses(self) -> None:
        periodic = ("vibeOS: time: lapic_timer ok (periodic)", *self.GOOD[1:])
        hpet = (self.GOOD[0], "vibeOS: time: clocksource hpet", *self.GOOD[2:])
        no_cs = (self.GOOD[0], *self.GOOD[2:])
        skip = (*self.GOOD[:4], "vibeOS: ktest: skip pit_tick_rate: why", *self.GOOD[5:])
        fail = (*self.GOOD[:4], "vibeOS: ktest: FAIL pit_tick_rate: no", *self.GOOD[5:])
        missing = (*self.GOOD[:2], "vibeOS: ktest: begin 1", "vibeOS: ktest: run x 10000",
                   "vibeOS: ktest: ok x (1 us)", "vibeOS: ktest: end")
        for why, lines in (
            ("periodic", periodic),
            ("hpet clocksource", hpet),
            ("no clocksource line", no_cs),
            ("skip", skip),
            ("FAIL", fail),
            ("missing", missing),
        ):
            with self.subTest(why=why):
                with self.assertRaisesRegex(HarnessError, "hpet=off boot"):
                    self.check(*lines)

    def _main(self, argv: list[str]) -> mock.MagicMock:
        first = _boot(
            *BLOCK_MARKERS,
            "vibeOS: ktest: begin 1",
            "vibeOS: ktest: run heap_box 10000",
            "vibeOS: ktest: ok heap_box (3 us)",
            *STACK_REPORT,
            "vibeOS: ktest: end",
        )
        env = {"VIBEOS_ISO": "x.iso", "VIBEOS_KTEST": "heap_box", "VIBEOS_QEMU_ACCEL": "tcg"}
        with (
            overlay_env(env, clear=True),
            mock.patch.object(results.Results, "write"),
            mock.patch.object(run_ktest, "run_qemu_until_exit", side_effect=[first]),
            mock.patch.object(run_ktest, "qemu_argv", return_value=["qemu"]),
            mock.patch.object(run_ktest, "hpet_off_boot") as off,
        ):
            self.assertEqual(run_ktest.main(argv), 0)
        return off

    def test_main_flag(self) -> None:
        self.assertEqual(self._main(["--hpet-off"]).call_count, 1)
        self.assertEqual(self._main([]).call_count, 0)


if __name__ == "__main__":
    unittest.main()
