"""Tests for run_forensics (ROADMAP §10.7, `make test-forensics`): the give-up
deadline over a fake line source and QMP, and each check on doctored input."""

from __future__ import annotations

import json
import struct
import tempfile
import unittest
from pathlib import Path

from tests.harness import frame, qmp
from tests.harness.harness import HarnessError, Marker, QemuConfig
from tests.harness.linesource import FakeLineSource
from tests.harness.qmp import FakeQmp
from tests.harness.run_forensics import (
    GIVE_UP_S,
    HANG_ARMED,
    boot_and_capture,
    check_export,
    check_gp_sig,
    check_report,
    check_virt,
    normalize,
    panic_line,
    trace_order,
)


def k(text: str) -> str:
    return frame.FRAME + text


MARKERS = [
    Marker("vibeOS: smp: done", "smp_done"),
    Marker(HANG_ARMED, "hang_test_armed"),
]

# The vectors of `vibeos::log::vmcore::tests::NORMALIZE_VECTORS`.
NORMALIZE_VECTORS = [
    ("", ""),
    ("no numbers", "no numbers"),
    (
        "index out of bounds: the len is 3 but the index is 17",
        "index out of bounds: the len is N but the index is N",
    ),
    ("#GP rip=0xffffffff80123abc cs=0x8 err=0x0", "#GP rip=N cs=N err=N"),
    ("u64 cpu1 0x 0xg 00x1f", "uN cpuN Nx Nxg NxNf"),
    ("DEADBEEF deadbeef 0XFF", "DEADBEEF deadbeef NXFF"),
    ("wait_acks late 12 s: cpu1", "wait_acks late N s: cpuN"),
    ("héllo 42 wörld", "héllo N wörld"),
]


def report(
    *,
    sig: str = "sig: timeout @ vibeos::smp::hang_test::hold < vibeos::smp::hang_test::arm "
    "< vibeos::boot_rest",
    cpus: int = 4,
    drop_thread: bool = False,
    log_n: int = 70,
    log_shown: int = 64,
    last: str = HANG_ARMED,
) -> str:
    """A hang core's report as the tool prints it."""
    out = [sig, "core: 151191552 RAM in 5 segments", "build-id: 00", "panic: none"]
    for c in range(cpus):
        cur = 0 if c == 0 else 8 + c
        out.append(f"cpu {c} apic {c} current {cur} idle {c + 1} runq [] regs prstatus running")
        if c == 0:
            out.append("  #0 0xffffffff80069bb2 vibeos::smp::hang_test::hold+0x12")
            out.append("  #1 0xffffffff80069b91 vibeos::smp::hang_test::arm+0x181")
        else:
            out.append("  #0 0xffffffff80069c32 vibeos::smp::hang_test::wait+0x72")
    tids = [0] + [8 + c for c in range(1, cpus)]
    if drop_thread:
        tids = tids[:-1]
    for t in tids:
        out.append(f"thread {t} running cpu 0 pid 0 rip=0x0 rsp=0x0 rbp=0x0")
    out.append(f"log: last {log_shown} of {log_n} (0 dropped)")
    for i in range(log_shown - 1):
        out.append(f"  [      {i}] cpu0 info vibeOS: line {i}")
    out.append(f"  [     145] cpu0 info {last}")
    out.append("trace: order per-cpu (tsc not invariant)")
    return "\n".join(out) + "\n"


class GiveUp(unittest.TestCase):
    def test_gives_up_five_seconds_after_marker(self) -> None:
        src = FakeLineSource(
            [
                ("line", k("vibeOS: smp: done")),
                ("line", k(HANG_ARMED)),
                ("idle", ""),
                ("timeout", ""),
            ],
            exit_code=None,
        )
        fake = FakeQmp([])
        taken: list[Path] = []

        def capture(q: qmp.QmpLike, out: Path) -> Path:
            taken.append(out)
            return out / "core.zst"

        with tempfile.TemporaryDirectory() as d:
            boot = boot_and_capture(
                QemuConfig(iso="build/vibeos-hang.iso", smp=4),
                MARKERS,
                Path(d) / "hang",
                "hang",
                60.0,
                line_source=src,
                qmp_client=fake,
                clock=lambda: 100.0,
                capture=capture,
            )
            wrote = sorted(p.name for p in (Path(d) / "hang").iterdir())
        self.assertEqual(boot.armed_at, 100.0)
        self.assertEqual(src.deadlines[-1], 100.0 + GIVE_UP_S)
        self.assertEqual(GIVE_UP_S, 5.0)
        self.assertEqual(boot.core, taken[0] / "core.zst")
        # Stopped, then the core, then quit.
        names = fake.names()
        self.assertLess(names.index("stop"), names.index("quit"))
        self.assertEqual(boot.result.matched, ["smp_done", "hang_test_armed"])
        self.assertIn("qemu-argv.txt", wrote)

    def test_panic_before_marker_fails(self) -> None:
        src = FakeLineSource(
            [
                ("line", k("vibeOS: smp: done")),
                ("line", k("vibeOS: panic: msg: boom")),
                ("line", k("vibeOS: panic: halted")),
                ("eof", ""),
            ],
            exit_code=None,
        )
        with tempfile.TemporaryDirectory() as d, self.assertRaises(HarnessError) as cm:
            boot_and_capture(
                QemuConfig(iso="build/vibeos-hang.iso", smp=4),
                MARKERS,
                Path(d) / "hang",
                "hang",
                60.0,
                line_source=src,
                qmp_client=FakeQmp([]),
                clock=lambda: 100.0,
                capture=lambda q, out: out / "core.zst",
            )
        self.assertIn("panic signature", str(cm.exception))
        # The give-up deadline was never armed.
        self.assertNotIn(100.0 + GIVE_UP_S, src.deadlines)

    def test_timeout_before_marker_fails(self) -> None:
        src = FakeLineSource([("line", k("vibeOS: smp: done")), ("timeout", "")], exit_code=None)
        with tempfile.TemporaryDirectory() as d, self.assertRaises(HarnessError) as cm:
            boot_and_capture(
                QemuConfig(iso="build/vibeos-hang.iso", smp=4),
                MARKERS,
                Path(d) / "hang",
                "hang",
                60.0,
                line_source=src,
                qmp_client=FakeQmp([]),
                clock=lambda: 100.0,
                capture=lambda q, out: out / "core.zst",
            )
        self.assertIn("missing 'hang_test_armed'", str(cm.exception))


class Checks(unittest.TestCase):
    def test_check_report_rejects_doctored_reports(self) -> None:
        check_report(report(), 4)
        check_report(report(log_n=10, log_shown=10), 4)
        bad = {
            "wrong sig": report(sig="sig: timeout @ a < b < c"),
            "a panic sig": report(sig="sig: boom @ vibeos::x < ? < ?"),
            "missing cpu": report(cpus=3),
            "missing thread": report(drop_thread=True),
            "short log tail": report(log_shown=63),
            "tail over the ring": report(log_n=10, log_shown=11),
            "last record": report(last="vibeOS: smp: done"),
        }
        for why, text in bad.items():
            with self.subTest(why), self.assertRaises(HarnessError):
                check_report(text, 4)
        # CPUs 1 up must be in `hang_test::wait`.
        no_wait = report().replace("hang_test::wait", "sched_init::idle_loop")
        with self.assertRaises(HarnessError):
            check_report(no_wait, 4)
        # A thread line with no state.
        with self.assertRaises(HarnessError):
            check_report(report() + "thread 99 lost\n", 4)
        self.assertEqual(trace_order(report()), "per-cpu")

    def _export(self, d: str, doc: object) -> Path:
        p = Path(d) / "trace.json"
        p.write_text(json.dumps(doc), encoding="utf-8")
        return p

    def test_check_export_rejects_missing_fields(self) -> None:
        def ev(cpu: int, **drop: bool) -> dict[str, object]:
            e: dict[str, object] = {"name": "switch", "ph": "i", "ts": 1.0, "pid": cpu, "tid": cpu}
            for key in drop:
                e.pop(key)
            return e

        events = [ev(c) for c in range(4)]
        other = {"order": "per-cpu"}
        good = {"traceEvents": events, "otherData": other}
        with tempfile.TemporaryDirectory() as d:
            check_export(self._export(d, good), 4, "per-cpu")
            for key in ("name", "ph", "ts", "pid", "tid"):
                doc = {"traceEvents": [ev(0, **{key: True}), *events[1:]], "otherData": other}
                with self.subTest(key), self.assertRaises(HarnessError):
                    check_export(self._export(d, doc), 4, "per-cpu")
            with self.assertRaises(HarnessError):
                check_export(self._export(d, {"traceEvents": [], "otherData": {}}), 4, "per-cpu")
            with self.assertRaises(HarnessError):
                check_export(self._export(d, good), 4, "global")
            three = {"traceEvents": events[:3], "otherData": other}
            with self.assertRaises(HarnessError):
                check_export(self._export(d, three), 4, "per-cpu")
            p = Path(d) / "bad.json"
            p.write_text("{", encoding="utf-8")
            with self.assertRaises(HarnessError):
                check_export(p, 4, "per-cpu")


def _elf(kind: int, loads: list[tuple[int, bytes]], text: tuple[int, bytes] | None) -> bytes:
    """An ELF64 x86-64 file: one PT_LOAD per `(vaddr, bytes)`, and with
    `text` a section table holding `.text` at that address."""
    phoff = 64
    body_at = phoff + 56 * len(loads)
    body = b""
    phdrs = b""
    for vaddr, data in loads:
        off = body_at + len(body)
        phdrs += struct.pack("<IIQQQQQQ", 1, 7, off, vaddr, 0, len(data), len(data), 0x1000)
        body += data
    shoff = 0
    shnum = 0
    shdrs = b""
    tail = b""
    if text is not None:
        addr, data = text
        text_off = body_at + len(body)
        body += data
        shstr = b"\0.text\0.shstrtab\0"
        str_off = body_at + len(body)
        body += shstr
        shoff = body_at + len(body)
        shnum = 3
        shdrs = bytes(64)
        shdrs += struct.pack("<IIQQQQIIQQ", 1, 1, 6, addr, text_off, len(data), 0, 0, 16, 0)
        shdrs += struct.pack("<IIQQQQIIQQ", 7, 3, 0, 0, str_off, len(shstr), 0, 0, 1, 0)
        tail = shdrs
    ehdr = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9)
    ehdr += struct.pack(
        "<HHIQQQIHHHHHH", kind, 62, 1, 0, phoff, shoff, 0, 64, 56, len(loads), 64, shnum, 2
    )
    return ehdr + phdrs + body + tail


class Virt(unittest.TestCase):
    def test_check_virt_on_synthetic_elf(self) -> None:
        text = b"\x55\x48\x89\xe5" * 256
        kbase = 0xFFFF_FFFF_8000_0000
        elf = _elf(2, [(kbase, text)], (kbase, text))
        whole = _elf(4, [(kbase - 0x1000, bytes(0x1000)), (kbase, text + bytes(0x1000))], None)
        split = _elf(4, [(kbase, text[:512]), (kbase + 512, text[512:])], None)
        short = _elf(4, [(kbase, text[:512])], None)
        wrong = _elf(4, [(kbase, bytes(len(text)))], None)
        with tempfile.TemporaryDirectory() as d:
            e = Path(d) / "kernel.elf"
            e.write_bytes(elf)
            for name, core in (("whole", whole), ("split", split)):
                v = Path(d) / name
                v.write_bytes(core)
                check_virt(v, e)
            for name, core in (("short", short), ("wrong", wrong)):
                v = Path(d) / name
                v.write_bytes(core)
                with self.subTest(name), self.assertRaises(HarnessError):
                    check_virt(v, e)


class Signature(unittest.TestCase):
    def test_normalize_vectors(self) -> None:
        for raw, want in NORMALIZE_VECTORS:
            with self.subTest(raw=raw):
                self.assertEqual(normalize(raw), want)

    def test_panic_line_cut(self) -> None:
        self.assertEqual(panic_line("a\nb"), "a")
        self.assertEqual(len(panic_line("x" * 200).encode()), 120)
        self.assertEqual(panic_line("x" * 119 + "é"), "x" * 119)
        self.assertEqual(panic_line("x" * 119 + " err=0x8"), "x" * 119)
        self.assertEqual(panic_line("boom \t\r\nnext"), "boom")

    def test_gp_signature(self) -> None:
        gp = "#GP rip=0xffffffff8002b6b8 cs=0x8 rflags=0x10046 rsp=0xffffd000 ss=0x10 err=0x8"
        lines = [k("vibeOS: boot: gp-test armed"), k(f"vibeOS: {gp}")]
        good = (
            f"sig: {normalize(gp)} @ vibeos::gp_test_fault < vibeos::gp_test_trip "
            "< vibeos::boot_rest\n"
        )
        check_gp_sig(good, lines)
        for bad in (
            good.replace("gp_test_trip", "gp_test_x"),
            good.replace(" @ ", " err @ "),
            "sig: timeout @ a < b < c\n",
        ):
            with self.subTest(bad), self.assertRaises(HarnessError):
                check_gp_sig(bad, lines)


if __name__ == "__main__":
    unittest.main()
