"""Unit tests for `tests/harness/qmp.py` (C-QMP, DESIGN §8.3's event rule).

`TestEventRuleReplays` replays the QMP event streams `run_qmp.py --record`
took from QEMU (`tests/harness/fixtures/qmp/`), once per declaration, so a
QEMU whose events change (one that pauses on `GUEST_CRASHLOADED`, say)
fails here once `make test-qmp` re-records them.
"""

from __future__ import annotations

import dataclasses
import json
import os
import shutil
import socket
import struct
import sys
import tempfile
import threading
import unittest
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from unittest import mock

from tests.harness import qmp, results
from tests.harness.harness import (
    PANIC_ACTION,
    EnvConfig,
    HarnessError,
    Marker,
    QemuConfig,
    overlay_env,
    qemu_argv,
    run_qemu_and_check,
    run_qemu_console_input,
    run_qemu_until_exit,
)
from tests.harness.linesource import FakeLineSource
from tests.harness.qmp import (
    NO_EVENT_CORE_S,
    VMCORE_WRITTEN,
    Decision,
    EventRule,
    FakeQmp,
    QmpClient,
    QmpError,
)

FIXTURES = Path(__file__).resolve().parent / "fixtures" / "qmp"
STREAMS = ("none", "panic", "reset", "reset-extra", "crashloaded")
# Copies stdin to the file named by argv[1]: the tests' stand-in for zstd.
COPY = (
    sys.executable,
    "-c",
    "import shutil, sys; shutil.copyfileobj(sys.stdin.buffer, open(sys.argv[1], 'wb'))",
)
# Reads stdin, then exits 3: a compressor that fails.
FAIL3 = (sys.executable, "-c", "import sys; sys.stdin.buffer.read(); sys.exit(3)")


def events(name: str) -> list[qmp.Event]:
    return qmp.load_stream(FIXTURES / f"{name}.jsonl")[1]


def replay(rule: EventRule, evs: list[qmp.Event]) -> list[tuple[str, Decision]]:
    """Each event's name with the decision the rule gave it, when it gave one."""
    out = []
    for ev in evs:
        d = rule.on_event(ev)
        if d:
            out.append((str(ev["event"]), d))
    return out


class Clock:
    def __init__(self) -> None:
        self.now = 100.0

    def __call__(self) -> float:
        return self.now


class TestFixtures(unittest.TestCase):
    def test_every_stream_has_a_header_and_no_timestamps(self) -> None:
        for name in STREAMS:
            with self.subTest(name=name):
                header, evs = qmp.load_stream(FIXTURES / f"{name}.jsonl")
                self.assertIn("query-version", header)
                argv = header["argv"]
                assert isinstance(argv, list)
                self.assertIn("pvpanic", argv)
                self.assertEqual(argv[argv.index("-action") + 1], "panic=pause")
                self.assertIn("-S", argv)
                self.assertEqual("-no-reboot" not in argv, name.startswith("reset"))
                self.assertTrue(evs)
                for ev in evs:
                    self.assertNotIn("timestamp", ev)
                self.assertEqual(evs[0]["event"], "RESUME")

    def test_no_stop_after_crashloaded(self) -> None:
        names = [e["event"] for e in events("crashloaded")]
        i = names.index("GUEST_CRASHLOADED")
        self.assertNotIn("STOP", names[i:])
        self.assertEqual(names[-1], "SHUTDOWN")

    def test_q35_lists_pvpanic_port(self) -> None:
        fw = json.loads((FIXTURES / "fwcfg-q35.json").read_text(encoding="utf-8"))
        self.assertIn("q35", fw["header"]["argv"])
        entry = [f for f in fw["files"] if f["name"] == "etc/pvpanic-port"]
        self.assertEqual(len(entry), 1)
        self.assertEqual(entry[0]["size"], 2)

    def test_names_escape_gitignore(self) -> None:
        for p in FIXTURES.iterdir():
            with self.subTest(p=p.name):
                self.assertFalse(p.name.startswith("core."))
                self.assertFalse(p.name.endswith(".core"))
                self.assertNotEqual(p.name, "core")


class TestEventRuleReplays(unittest.TestCase):
    """Each declaration over each recorded stream."""

    def test_none(self) -> None:
        self.assertEqual(replay(EventRule("none"), events("none")), [])
        for name in ("panic", "reset", "reset-extra"):
            with self.subTest(name=name):
                got = replay(EventRule("none"), events(name))
                self.assertEqual(got[0][0], "GUEST_PANICKED")
                d = got[0][1]
                self.assertEqual((d.end, d.core, d.stop_first), ("fail", True, True))
                self.assertEqual(len(got), 1, "the rule decides once")
        got = replay(EventRule("none"), events("crashloaded"))
        self.assertEqual(
            [(n, d.end, d.core) for n, d in got], [("GUEST_CRASHLOADED", "fail", True)]
        )

    def test_panic(self) -> None:
        got = replay(EventRule("panic"), events("panic"))
        self.assertEqual([(n, d.end, d.core) for n, d in got], [("GUEST_PANICKED", "pass", False)])
        rule = EventRule("panic")
        self.assertEqual(replay(rule, events("none")), [])
        d = rule.on_exit()
        self.assertEqual((d.end, d.core), ("fail", False))
        self.assertIn("before GUEST_PANICKED", d.reason)
        got = replay(EventRule("panic"), events("crashloaded"))
        self.assertEqual(
            [(n, d.end, d.core) for n, d in got], [("GUEST_CRASHLOADED", "fail", True)]
        )

    def test_reset(self) -> None:
        got = replay(EventRule("reset", 1), events("reset"))
        self.assertEqual([(n, d.cont, d.end) for n, d in got], [("GUEST_PANICKED", True, None)])
        got = replay(EventRule("reset", 1), events("reset-extra"))
        self.assertEqual(
            [(n, d.cont, d.end, d.core) for n, d in got],
            [("GUEST_PANICKED", True, None, False), ("RESET", False, "fail", True)],
        )
        self.assertIn("RESET 2, the run declared resets=1", got[1][1].reason)
        got = replay(EventRule("reset", 2), events("reset-extra"))
        self.assertEqual([n for n, _ in got], ["GUEST_PANICKED"])
        got = replay(EventRule("reset", 0), events("reset"))
        self.assertEqual(got[-1][1].end, "fail")
        got = replay(EventRule("reset", 1), events("crashloaded"))
        self.assertEqual(
            [(n, d.end, d.core) for n, d in got], [("GUEST_CRASHLOADED", "fail", True)]
        )

    def test_capture(self) -> None:
        # The capture kernel's line comes between the event and the reset.
        rule = EventRule("capture")
        evs = events("crashloaded")
        i = [e["event"] for e in evs].index("GUEST_CRASHLOADED")
        self.assertEqual(replay(rule, evs[: i + 1]), [])
        self.assertTrue(rule.waiting)
        rule.on_line(f"{VMCORE_WRITTEN}1048576 bytes", panic=False, halted=False)
        got = replay(rule, evs[i + 1 :])
        self.assertEqual([(n, d.end) for n, d in got], [("SHUTDOWN", "pass")])
        self.assertFalse(rule.waiting)
        # No vmcore line: the reset fails the run, with no core to take.
        got = replay(EventRule("capture"), evs)
        self.assertEqual([(n, d.end, d.core) for n, d in got], [("SHUTDOWN", "fail", False)])
        got = replay(EventRule("capture"), events("panic"))
        self.assertEqual([(n, d.end, d.core) for n, d in got], [("GUEST_PANICKED", "fail", True)])

    def test_qemu_that_pauses_on_crashloaded_fails_capture(self) -> None:
        evs = events("crashloaded")
        i = [e["event"] for e in evs].index("GUEST_CRASHLOADED")
        paused = evs[: i + 1] + [{"event": "STOP"}] + evs[i + 1 :]
        got = replay(EventRule("capture"), paused)
        self.assertEqual([(n, d.end, d.core) for n, d in got], [("STOP", "fail", True)])
        self.assertIn("-action panic=none", got[0][1].reason)

    def test_no_event_signature_takes_core_after_halted(self) -> None:
        clock = Clock()
        rule = EventRule("none", clock=clock)
        d = rule.on_line("\x1evibeOS: panic:", panic=True, halted=False)
        self.assertEqual((d.end, d.core), ("fail", True))
        self.assertTrue(rule.core_pending)
        rule.on_line("\x1evibeOS: backtrace:", panic=False, halted=False)
        self.assertTrue(rule.core_pending)
        rule.on_line("\x1evibeOS: panic: halted", panic=False, halted=True)
        self.assertFalse(rule.core_pending)

    def test_no_event_signature_takes_core_after_10_s(self) -> None:
        clock = Clock()
        rule = EventRule("none", clock=clock)
        rule.on_line("\x1evibeOS: panic:", panic=True, halted=False)
        clock.now += NO_EVENT_CORE_S - 0.5
        rule.on_tick()
        self.assertTrue(rule.core_pending)
        clock.now += 1.0
        rule.on_tick()
        self.assertFalse(rule.core_pending)

    def test_no_event_signature_event_ends_the_wait(self) -> None:
        rule = EventRule("none", clock=Clock())
        rule.on_line("\x1evibeOS: panic:", panic=True, halted=False)
        self.assertEqual(replay(rule, events("panic")), [])
        self.assertFalse(rule.core_pending)

    def test_signatures_expected_elsewhere(self) -> None:
        for expect in ("panic", "reset", "capture"):
            with self.subTest(expect=expect):
                rule = EventRule(expect)
                self.assertFalse(rule.on_line("\x1evibeOS: panic:", panic=True, halted=False))
                self.assertFalse(rule.core_pending)

    def test_timeout_takes_a_core(self) -> None:
        for expect in qmp.EXPECTS:
            with self.subTest(expect=expect):
                d = EventRule(expect).on_timeout()
                self.assertEqual((d.end, d.core, d.stop_first), ("fail", True, True))
        rule = EventRule("panic")
        rule.on_line("\x1evibeOS: panic: halted", panic=False, halted=True)
        self.assertIn("no GUEST_PANICKED within 10 s", rule.on_timeout().reason)

    def test_aarch64_panic_ends_at_halted(self) -> None:
        rule = EventRule("panic", arch="aarch64")
        d = rule.on_line("\x1evibeOS: panic: halted", panic=False, halted=True)
        self.assertEqual((d.end, d.reason), ("pass", "PANIC_DONE"))
        self.assertEqual(rule.ended, "pass")

    def test_bad_declaration(self) -> None:
        with self.assertRaises(HarnessError):
            EventRule("maybe")
        with self.assertRaises(HarnessError):
            EventRule("reset", -1)


def _server(handler: object) -> tuple[str, threading.Thread]:
    d = tempfile.mkdtemp(prefix="vibeos-qmpt-")
    path = os.path.join(d, "s")
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(1)

    def run() -> None:
        conn, _ = srv.accept()
        try:
            handler(conn)  # type: ignore[operator]
        finally:
            conn.close()
            srv.close()
            shutil.rmtree(d, ignore_errors=True)

    t = threading.Thread(target=run, daemon=True)
    t.start()
    return path, t


def _send(conn: socket.socket, obj: object) -> None:
    conn.sendall((json.dumps(obj) + "\r\n").encode())


def _recv(f: object) -> dict[str, object]:
    line = f.readline()  # type: ignore[attr-defined]
    msg = json.loads(line)
    assert isinstance(msg, dict)
    return msg


class TestQmpClient(unittest.TestCase):
    def test_connect_negotiates_and_matches_ids(self) -> None:
        seen: list[dict[str, object]] = []

        def handler(conn: socket.socket) -> None:
            f = conn.makefile("rb")
            _send(conn, {"QMP": {"version": {}, "capabilities": []}})
            m = _recv(f)
            seen.append(m)
            _send(conn, {"return": {}, "id": m["id"]})
            m = _recv(f)
            seen.append(m)
            _send(conn, {"event": "RESUME", "timestamp": {}})
            _send(conn, {"return": {}, "id": "someone-else"})
            _send(conn, {"return": {"status": "running"}, "id": m["id"]})
            _send(conn, {"event": "STOP", "timestamp": {}})
            _send(conn, {"event": "GUEST_PANICKED", "data": {"action": "pause"}})
            m = _recv(f)
            _send(conn, {"error": {"class": "GenericError", "desc": "nope"}, "id": m["id"]})
            f.read()

        path, t = _server(handler)
        c = QmpClient.connect(path)
        try:
            self.assertEqual(c.execute("query-status"), {"status": "running"})
            self.assertEqual(seen[0]["execute"], "qmp_capabilities")
            self.assertEqual(seen[1], {"execute": "query-status", "id": "vibeos-2"})
            ev = c.wait_event(("GUEST_PANICKED",), 5.0)
            assert ev is not None
            self.assertEqual(ev["data"], {"action": "pause"})
            self.assertEqual([e["event"] for e in c.poll()], ["RESUME", "STOP"])
            self.assertEqual(c.poll(), [])
            with self.assertRaisesRegex(QmpError, "GenericError: nope"):
                c.execute("bad")
        finally:
            c.close()
        t.join(5.0)

    def test_fds_ride_the_command(self) -> None:
        got: list[bytes] = []

        def handler(conn: socket.socket) -> None:
            _send(conn, {"QMP": {}})
            f = conn.makefile("rb")
            m = _recv(f)
            _send(conn, {"return": {}, "id": m["id"]})
            msg, fds, _flags, _addr = socket.recv_fds(conn, 4096, 1)
            got.append(msg)
            with os.fdopen(fds[0], "wb") as w:
                w.write(b"through the fd")
            _send(conn, {"return": {}, "id": json.loads(msg)["id"]})
            conn.recv(1)

        path, t = _server(handler)
        c = QmpClient.connect(path)
        r, w = os.pipe()
        try:
            c.execute("getfd", {"fdname": "x"}, fds=[w])
            os.close(w)
            with os.fdopen(r, "rb") as rf:
                self.assertEqual(rf.read(), b"through the fd")
        finally:
            c.close()
        t.join(5.0)
        self.assertEqual(json.loads(got[0])["arguments"], {"fdname": "x"})

    def test_connect_fails_within_timeout(self) -> None:
        d = tempfile.mkdtemp(prefix="vibeos-qmpt-")
        try:
            with self.assertRaisesRegex(QmpError, "no connection"):
                QmpClient.connect(os.path.join(d, "absent"), timeout_s=0.2)
        finally:
            shutil.rmtree(d)

    def test_socket_path_is_short_and_private(self) -> None:
        p = qmp.socket_path()
        try:
            self.assertLess(len(p), 104)
            self.assertTrue(os.path.basename(os.path.dirname(p)).startswith("vibeos-qmp-"))
            self.assertEqual(os.stat(os.path.dirname(p)).st_mode & 0o077, 0)
        finally:
            shutil.rmtree(os.path.dirname(p))


def elf_core(ncpu: int, info: bytes = b"OSRELEASE=0.8.0\nBUILD-ID=abcd\n") -> bytes:
    """A small ELF64 x86_64 core: one PT_NOTE with `ncpu` CORE NT_PRSTATUS
    notes and a VMCOREINFO note, then one PT_LOAD of zeros."""

    def pad(b: bytes) -> bytes:
        return b + b"\0" * (-len(b) % 4)

    def note(name: bytes, ntype: int, desc: bytes) -> bytes:
        n = name + b"\0"
        return struct.pack("<III", len(n), len(desc), ntype) + pad(n) + pad(desc)

    notes = b"".join(note(b"CORE", 1, b"\x11" * 336) for _ in range(ncpu))
    notes += note(b"VMCOREINFO", 0, info)
    phoff, phnum = 64, 2
    note_off = phoff + 56 * phnum
    load_off = note_off + len(notes)
    ehdr = b"\x7fELF" + bytes([2, 1, 1, 0]) + b"\0" * 8
    ehdr += struct.pack("<HHIQQQIHHHHHH", 4, 62, 1, 0, phoff, 0, 0, 64, 56, phnum, 0, 0, 0)
    ph_note = struct.pack("<IIQQQQQQ", 4, 0, note_off, 0, 0, len(notes), 0, 0)
    ph_load = struct.pack("<IIQQQQQQ", 1, 0, load_off, 0, 0, 4096, 4096, 0)
    return ehdr + ph_note + ph_load + notes + b"\0" * 4096


@contextmanager
def cores_in_tmp(tier: str = "test-x") -> Iterator[Path]:
    d = Path(tempfile.mkdtemp(prefix="vibeos-cores-"))
    try:
        with mock.patch.object(qmp, "CORES_DIR", d), mock.patch.object(
            results, "_current", results.Results(tier, out_dir=d / "results")
        ):
            yield d
    finally:
        shutil.rmtree(d, ignore_errors=True)


class TestTakeCore(unittest.TestCase):
    def test_pipe_plumbing(self) -> None:
        dump = elf_core(2)
        fake = FakeQmp([], dump=dump)
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "run"
            core = qmp.take_core(fake, out, compress=(*COPY, str(out / "core.zst")))
            self.assertEqual(core, out / "core.zst")
            self.assertEqual(core.read_bytes(), dump)
        self.assertEqual(fake.names(), ["getfd", "dump-guest-memory"])
        self.assertEqual(fake.commands[0][1], {"fdname": "vibeos-core"})
        self.assertEqual(
            fake.commands[1][1],
            {"paging": False, "protocol": "fd:vibeos-core", "detach": True},
        )

    def test_compressor_failure_fails(self) -> None:
        fake = FakeQmp([], dump=b"x" * 10)
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaisesRegex(QmpError, "exited 3"):
                qmp.take_core(fake, Path(d), compress=(*FAIL3,))

    def test_dump_refused_fails(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaisesRegex(QmpError, "no dump"):
                qmp.take_core(FakeQmp([]), Path(d), compress=(*COPY, os.path.join(d, "c")))

    def test_failed_dump_event_fails(self) -> None:
        class Failing(FakeQmp):
            def wait_event(self, names: object, timeout_s: float) -> qmp.Event | None:
                data = {"result": {"status": "failed"}, "error": "EIO"}
                return {"event": "DUMP_COMPLETED", "data": data}

        with tempfile.TemporaryDirectory() as d:
            with self.assertRaisesRegex(QmpError, "dump failed: EIO"):
                c = (*COPY, os.path.join(d, "c"))
                qmp.take_core(Failing([], dump=b"x"), Path(d), compress=c)

    def test_zstd_core_notes(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            core = qmp.take_core(FakeQmp([], dump=elf_core(3)), Path(d))
            notes = qmp.core_notes(core)
        prstatus = [n for n in notes if n[:2] == ("CORE", qmp.NT_PRSTATUS)]
        self.assertEqual(len(prstatus), 3)
        info = [n for n in notes if n[0] == "VMCOREINFO"]
        self.assertEqual(len(info), 1)
        self.assertIn(b"BUILD-ID=abcd", info[0][2])

    def test_parse_notes_refuses_other_cores(self) -> None:
        with self.assertRaisesRegex(QmpError, "not an ELF"):
            qmp.parse_notes(b"\0" * 64)
        core = bytearray(elf_core(1))
        core[18] = 183
        with self.assertRaisesRegex(QmpError, "e_machine 183"):
            qmp.parse_notes(bytes(core))

    def test_run_dir_and_files(self) -> None:
        with cores_in_tmp("test-e2e-panic") as root:
            a = qmp.run_dir("boot")
            b = qmp.run_dir("console")
            self.assertEqual(a, root / "x86_64-test-e2e-panic" / "001-boot")
            self.assertEqual(b, root / "x86_64-test-e2e-panic" / "002-console")
            iso = root / "vibeos-panic.iso"
            (root / "kernels").mkdir()
            (root / "kernels" / "vibeos-panic.elf").write_bytes(b"\x7fELF kernel")
            qmp.save_run_files(a, ["qemu-system-x86_64", "-cdrom", "a b.iso"], str(iso))
            self.assertEqual((a / "kernel.elf").read_bytes(), b"\x7fELF kernel")
            self.assertEqual(
                (a / "qemu-argv.txt").read_text(), "qemu-system-x86_64 -cdrom 'a b.iso'\n"
            )
            qmp.save_run_files(b, ["q"], str(root / "vibeos.iso"))
            self.assertIn("vibeos-default.elf", (b / "kernel.elf.missing").read_text())

    def test_kernel_elf_for(self) -> None:
        self.assertEqual(
            qmp.kernel_elf_for("build/vibeos.iso"), Path("build/kernels/vibeos-default.elf")
        )
        self.assertEqual(
            qmp.kernel_elf_for("build/vibeos-panic-stop.iso"),
            Path("build/kernels/vibeos-panic-stop.elf"),
        )


class TestQemuArgvQmp(unittest.TestCase):
    def test_default_argv(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso"), None)
        i = argv.index("-action")
        self.assertEqual(argv[i : i + 2], list(PANIC_ACTION))
        self.assertIn("-no-reboot", argv)
        self.assertNotIn("-qmp", argv)
        self.assertNotIn("-S", argv)

    def test_qmp_socket_and_halted_start(self) -> None:
        argv = qemu_argv(QemuConfig(iso="x.iso"), "/tmp/mon", qmp_sock="/tmp/q/qmp.sock")
        i = argv.index("-qmp")
        self.assertEqual(argv[i + 1], "unix:/tmp/q/qmp.sock,server=on,wait=off")
        self.assertIn("-S", argv)
        self.assertEqual(argv.count("-action"), 1)

    def test_reset_run_boots_without_no_reboot(self) -> None:
        self.assertNotIn("-no-reboot", qemu_argv(QemuConfig(iso="x.iso", expect="reset"), None))
        for expect in ("none", "panic", "capture"):
            with self.subTest(expect=expect):
                self.assertIn("-no-reboot", qemu_argv(QemuConfig(iso="x.iso", expect=expect), None))

    def test_env_passes_the_declaration(self) -> None:
        env = EnvConfig(
            iso="x.iso", smp=2, cpu="max", mem="128M", firmware=None, accel="tcg",
            timeout=60.0, extra=(),
        )
        cfg = env.qemu(expect="reset", resets=2)
        self.assertEqual((cfg.expect, cfg.resets), ("reset", 2))
        self.assertEqual(env.qemu().expect, "none")


def K(text: str) -> str:
    """A kernel line: framed (DESIGN §2.6)."""
    return "\x1e" + text


PANICKED: qmp.Event = {"event": "GUEST_PANICKED", "data": {"action": "pause"}}
FAKE_CFG = QemuConfig(iso="fake.iso")
ONLINE = "vibeOS: serial online"
DUMP = [K("vibeOS: panic:"), K("vibeOS: panic: msg: boom"), K("vibeOS: panic: halted")]


class TestRunnerHooks(unittest.TestCase):
    """The three runners drive a `qmp.Session` (C-QMP): `cont` at spawn,
    the event rule on each line and when idle, a core at a timeout or an
    undeclared panic, and the expected panic's `GUEST_PANICKED` end."""

    def core_dir(self, root: Path) -> Path:
        dirs = sorted(root.glob("*/*-*"))
        self.assertEqual(len(dirs), 1, dirs)
        return dirs[0]

    def test_check_conts_first_and_quits_on_the_contract(self) -> None:
        fake = FakeQmp([])
        src = FakeLineSource.from_lines([K(ONLINE)])
        result = run_qemu_and_check(FAKE_CFG, [Marker(ONLINE, "a")], line_source=src, qmp=fake)
        self.assertEqual(result.matched, ["a"])
        self.assertEqual(fake.names(), ["cont"])
        self.assertTrue(src.quit_sent)
        self.assertTrue(fake.closed)

    def test_check_undeclared_panic_event_takes_core(self) -> None:
        fake = FakeQmp([], after_line={1: [PANICKED]}, dump=elf_core(2))
        src = FakeLineSource.from_lines([K(ONLINE), K("vibeOS: x")], end="timeout", exit_code=None)
        with cores_in_tmp("test-e2e") as root:
            with self.assertRaises(HarnessError) as cm:
                run_qemu_and_check(
                    FAKE_CFG, [Marker(ONLINE, "a"), Marker("never", "b")], line_source=src, qmp=fake
                )
            d = self.core_dir(root)
            self.assertEqual(sorted(p.name for p in d.iterdir())[0], "core.zst")
            self.assertTrue((d / "qemu-argv.txt").is_file())
        msg = str(cm.exception)
        self.assertIn("GUEST_PANICKED in a run declared expect=none", msg)
        self.assertIn("--- guest core:", msg)
        self.assertEqual(fake.names(), ["cont", "stop", "getfd", "dump-guest-memory", "quit"])
        self.assertTrue(src.killed)

    def test_check_signature_core_follows_halted(self) -> None:
        fake = FakeQmp([], dump=elf_core(1))
        lines = [K(ONLINE), *DUMP, K("after the dump")]
        src = FakeLineSource.from_lines(lines, end="timeout", exit_code=None)
        with cores_in_tmp():
            with self.assertRaises(HarnessError) as cm:
                run_qemu_and_check(
                    FAKE_CFG, [Marker(ONLINE, "a"), Marker("never", "b")], line_source=src, qmp=fake
                )
        msg = str(cm.exception)
        self.assertIn("panic signature", msg)
        # The core waited for the dump's end, and read no further.
        self.assertIn("panic: msg: boom", msg)
        self.assertIn("vibeOS: panic: halted", msg)
        self.assertNotIn("after the dump", msg)
        self.assertEqual(src.next_event(), ("line", K("after the dump")))
        self.assertIn("dump-guest-memory", fake.names())

    def test_check_timeout_takes_core(self) -> None:
        fake = FakeQmp([], dump=elf_core(1))
        src = FakeLineSource.from_lines([K(ONLINE)], end="timeout", exit_code=None)
        with cores_in_tmp():
            with self.assertRaises(HarnessError) as cm:
                run_qemu_and_check(
                    FAKE_CFG, [Marker(ONLINE, "a"), Marker("never", "b")], line_source=src, qmp=fake
                )
        self.assertIn("timed out after", str(cm.exception))
        self.assertIn("--- guest core:", str(cm.exception))
        self.assertEqual(fake.names()[:2], ["cont", "stop"])

    def test_check_expected_panic_ends_on_event(self) -> None:
        fake = FakeQmp([], after_line={4: [PANICKED]})
        src = FakeLineSource.from_lines([K(ONLINE), *DUMP], exit_code=0)
        cfg = dataclasses.replace(FAKE_CFG, expect="panic")
        result = run_qemu_and_check(cfg, [Marker(ONLINE, "a")], line_source=src, qmp=fake)
        self.assertEqual(result.end, "GUEST_PANICKED")
        # The paused guest is quit through QMP once the checks pass.
        self.assertEqual(fake.names(), ["cont", "quit"])
        self.assertFalse(src.killed)

    def test_check_expected_panic_event_before_dump_lines(self) -> None:
        # QMP can beat the serial pipe: the dump's tail is drained after it.
        fake = FakeQmp([], after_line={2: [PANICKED]})
        src = FakeLineSource.from_lines([K(ONLINE), *DUMP], exit_code=None)
        cfg = dataclasses.replace(FAKE_CFG, expect="panic")
        result = run_qemu_and_check(
            cfg, [Marker(ONLINE, "a")], line_source=src, qmp=fake, dump_needles=("halted",)
        )
        self.assertEqual(result.lines[-1], K("vibeOS: panic: halted"))

    def test_console_idle_event_takes_core(self) -> None:
        fake = FakeQmp([PANICKED], dump=elf_core(1))
        src = FakeLineSource([("line", K(ONLINE)), ("idle", "")], exit_code=None)
        with cores_in_tmp("test-ps2") as root:
            with self.assertRaises(HarnessError) as cm:
                run_qemu_console_input(FAKE_CFG, line_source=src, qmp=fake)
            self.assertIn("-console", self.core_dir(root).name)
        self.assertIn("GUEST_PANICKED", str(cm.exception))
        self.assertEqual(fake.names()[0], "cont")

    def test_console_user_failure_takes_no_core(self) -> None:
        fake = FakeQmp([], dump=elf_core(1))
        src = FakeLineSource.from_lines([K(ONLINE), "user: tests fail x"], exit_code=None)
        with cores_in_tmp() as root:
            with self.assertRaises(HarnessError):
                run_qemu_console_input(FAKE_CFG, line_source=src, qmp=fake)
            self.assertEqual(list(root.glob("*/*-*")), [])
        self.assertNotIn("dump-guest-memory", fake.names())

    def _script(self, d: str, body: str) -> tuple[QemuConfig, str]:
        qemu = os.path.join(d, "qemu-system-x86_64")
        with open(qemu, "w", encoding="utf-8") as f:
            f.write("#!/bin/sh\n" + body)
        os.chmod(qemu, 0o755)
        iso = os.path.join(d, "x.iso")
        open(iso, "wb").close()
        return QemuConfig(iso=iso, accel=""), d + os.pathsep + os.environ.get("PATH", "")

    def test_until_exit_undeclared_panic_takes_core(self) -> None:
        fake = FakeQmp([], dump=elf_core(2))
        with tempfile.TemporaryDirectory() as d, cores_in_tmp("test-kernel") as root:
            cfg, path = self._script(
                d,
                "printf '\\036vibeOS: panic:\\n\\036vibeOS: panic: halted\\n'\nexec sleep 30\n",
            )
            with overlay_env({"PATH": path}):
                with self.assertRaises(HarnessError) as cm:
                    run_qemu_until_exit(cfg, timeout_s=20, qmp=fake)
            self.assertTrue((self.core_dir(root) / "core.zst").is_file())
        self.assertIn("panic signature", str(cm.exception))
        self.assertEqual(fake.names(), ["cont", "stop", "getfd", "dump-guest-memory", "quit"])

    def test_until_exit_expected_panic_ends_on_event(self) -> None:
        fake = FakeQmp([], after_line={2: [PANICKED]})
        with tempfile.TemporaryDirectory() as d:
            cfg, path = self._script(
                d,
                "printf '\\036vibeOS: panic:\\n\\036vibeOS: panic: halted\\n'\nexec sleep 30\n",
            )
            cfg = dataclasses.replace(cfg, expect="panic")
            with overlay_env({"PATH": path}):
                r = run_qemu_until_exit(cfg, timeout_s=20, qmp=fake)
        self.assertEqual(r.end, "GUEST_PANICKED")
        self.assertEqual(fake.names(), ["cont", "quit"])
