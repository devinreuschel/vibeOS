"""QEMU's QMP for the harness: events, the event rule and guest cores (C-QMP).

DESIGN §8.3 (TESTING.md §8.3) and ROADMAP §10.7. Every harness boot starts
QEMU halted (`-S`) with a QMP socket, connects, negotiates, and only then
sends `cont`, so no event is lost. Each run declares the end it expects,
`QemuConfig.expect`, one of `EXPECTS`:

- `none`: `GUEST_PANICKED` or `GUEST_CRASHLOADED` stops the guest, takes a
  core, quits and fails; a panic signature with no event fails at once,
  with the core after `vibeOS: panic: halted` or `NO_EVENT_CORE_S`.
- `panic`: `GUEST_PANICKED` drains serial and passes, and the runner checks
  the dump (a core only if a check fails); `GUEST_CRASHLOADED` fails.
- `reset`: `GUEST_PANICKED` gets `cont`; `GUEST_CRASHLOADED` fails.
- `capture`: `GUEST_PANICKED` fails; `GUEST_CRASHLOADED` waits for the
  capture kernel's `vibeOS: vmcore: written` line and its reset, and a
  `STOP` after it fails the run (a QEMU that pauses there).

A `reset` run boots without `-no-reboot` and fails, with a core, on a
`RESET` beyond `QemuConfig.resets`. Every timeout takes a core. `reset` and
`capture` runs take one only when they fail.

A core is `dump-guest-memory` with `"paging": false` and no format: a
physical ELF core with one `NT_PRSTATUS` note per CPU and the kernel's
VMCOREINFO note, never `-p` or a kdump format. QEMU writes it to the write
end of a pipe it receives through `getfd`, and host `zstd` compresses the
other end, so no uncompressed core touches the disk (`take_core`). Each
failed run's directory, `build/cores/<arch>-<tier>/<seq>-<label>/`, holds
`core.zst`, `kernel.elf` (C-BUILD-OUTPUTS) and `qemu-argv.txt`.

`EventRule` only decides: the runners pass whether a line is a panic
signature and whether it is `vibeOS: panic: halted`; nothing here matches a
signature. `tier` and `arch` come from `results.current()`; nothing here
reads a `VIBEOS_*` variable.

Sources, documentation only (DESIGN §1.5): QEMU's `docs/interop/qmp-spec.rst`
(greeting, `qmp_capabilities`, `id`, events) and `qapi/dump.json`,
`qapi/run-state.json` and `qapi/misc.json` (`dump-guest-memory`,
`DUMP_COMPLETED`, `GUEST_PANICKED`, `GUEST_CRASHLOADED`, `getfd`); the ELF
core layout is the System V gABI's.

Standard library only.
"""

from __future__ import annotations

import json
import os
import re
import select
import shlex
import shutil
import socket
import struct
import subprocess
import tempfile
import time
from collections import deque
from collections.abc import Callable, Collection, Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, NoReturn, Protocol

from tests.harness import results
from tests.harness.frame import kernel_text
from tests.harness.harness import (
    PANIC_DONE,
    HarnessError,
    QemuConfig,
    RunResult,
    serial_tail,
)

EXPECTS = ("none", "panic", "reset", "capture")
# A run that expects no panic and sees a signature with no event takes its
# core after `vibeOS: panic: halted` or this long, whichever is first.
NO_EVENT_CORE_S = 10.0
# The capture kernel's line once the vmcore is written (ROADMAP §25.4).
VMCORE_WRITTEN = "vibeOS: vmcore: written "
# The fd name `take_core` hands QEMU through `getfd`.
CORE_FD_NAME = "vibeos-core"
# Serial read after an ending event, for the dump's last lines (Risk 3: the
# serial pipe and the QMP socket race).
DRAIN_S = 0.5
REPO = Path(__file__).resolve().parents[2]
# `run_dir`'s root; unit tests point it at a temporary directory.
CORES_DIR = REPO / "build" / "cores"
EM_X86_64 = 62
NT_PRSTATUS = 1
_SAFE = re.compile(r"[^A-Za-z0-9._-]")

Event = dict[str, object]


class QmpError(HarnessError):
    """A QMP connection, command or dump failed."""


class QmpLike(Protocol):
    def execute(
        self,
        cmd: str,
        args: dict[str, object] | None = None,
        *,
        fds: Sequence[int] = (),
        timeout_s: float = 10.0,
    ) -> object:
        """Run `cmd` and return its `return` value; `QmpError` on an error reply."""
        ...

    def poll(self) -> list[Event]:
        """Every event received so far and not yet returned; never blocks."""
        ...

    def wait_event(self, names: Collection[str], timeout_s: float) -> Event | None:
        """The next event named in `names` (others stay queued), or None."""
        ...

    def close(self) -> None: ...


class QmpClient:
    """A QMP client over a Unix socket: commands matched by `id`, events queued."""

    def __init__(self, sock: socket.socket) -> None:
        self._sock = sock
        self._buf = bytearray()
        self._events: deque[Event] = deque()
        self._next_id = 0
        self._eof = False

    @classmethod
    def connect(cls, path: str, timeout_s: float = 5.0) -> QmpClient:
        """Connect to QEMU's `-qmp unix:<path>,server=on,wait=off`, read the
        greeting and negotiate; `QmpError` within `timeout_s` otherwise."""
        deadline = time.monotonic() + timeout_s
        while True:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                s.connect(path)
                break
            except OSError as e:
                s.close()
                if time.monotonic() >= deadline:
                    raise QmpError(
                        f"QMP: no connection to {path} within {timeout_s:g} s: {e}"
                    ) from e
                time.sleep(0.02)
        client = cls(s)
        try:
            greeting = client._read(deadline)
            if greeting is None or "QMP" not in greeting:
                raise QmpError(f"QMP: no greeting within {timeout_s:g} s: {greeting!r}")
            client.execute("qmp_capabilities", timeout_s=max(deadline - time.monotonic(), 1.0))
        except BaseException:
            client.close()
            raise
        return client

    def fileno(self) -> int:
        return self._sock.fileno()

    def _pop(self) -> Event | None:
        i = self._buf.find(b"\n")
        if i < 0:
            return None
        raw = bytes(self._buf[:i]).strip()
        del self._buf[: i + 1]
        if not raw:
            return None
        try:
            msg = json.loads(raw)
        except ValueError as e:
            raise QmpError(f"QMP: bad JSON {raw[:200]!r}") from e
        if not isinstance(msg, dict):
            raise QmpError(f"QMP: not an object: {raw[:200]!r}")
        return msg

    def _fill(self, wait_s: float) -> bool:
        """Read what the socket has within `wait_s`; False at EOF."""
        if self._eof:
            return False
        r, _, _ = select.select([self._sock], [], [], max(wait_s, 0.0))
        if not r:
            return True
        try:
            chunk = self._sock.recv(65536)
        except OSError:
            chunk = b""
        if not chunk:
            self._eof = True
            return False
        self._buf.extend(chunk)
        return True

    def _read(self, deadline: float) -> Event | None:
        """The next message by `deadline`; None at the deadline or EOF."""
        while True:
            while b"\n" in self._buf:
                msg = self._pop()
                if msg is not None:
                    return msg
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self._fill(remaining):
                return None

    def execute(
        self,
        cmd: str,
        args: dict[str, object] | None = None,
        *,
        fds: Sequence[int] = (),
        timeout_s: float = 10.0,
    ) -> object:
        self._next_id += 1
        mid = f"vibeos-{self._next_id}"
        msg: dict[str, object] = {"execute": cmd, "id": mid}
        if args is not None:
            msg["arguments"] = args
        data = (json.dumps(msg) + "\n").encode()
        try:
            if fds:
                sent = socket.send_fds(self._sock, [data], list(fds))
                if sent < len(data):
                    self._sock.sendall(data[sent:])
            else:
                self._sock.sendall(data)
        except OSError as e:
            raise QmpError(f"QMP {cmd}: send failed: {e}") from e
        deadline = time.monotonic() + timeout_s
        while True:
            reply = self._read(deadline)
            if reply is None:
                why = "connection closed" if self._eof else f"no reply within {timeout_s:g} s"
                raise QmpError(f"QMP {cmd}: {why}")
            if "event" in reply:
                self._events.append(reply)
                continue
            if reply.get("id") != mid:
                continue
            err = reply.get("error")
            if err is not None:
                cls = err.get("class") if isinstance(err, dict) else None
                desc = err.get("desc") if isinstance(err, dict) else err
                raise QmpError(f"QMP {cmd}: {cls}: {desc}")
            return reply.get("return")

    def poll(self) -> list[Event]:
        while self._fill(0.0):
            r, _, _ = select.select([self._sock], [], [], 0)
            if not r:
                break
        while b"\n" in self._buf:
            msg = self._pop()
            if msg is not None and "event" in msg:
                self._events.append(msg)
        out = list(self._events)
        self._events.clear()
        return out

    def wait_event(self, names: Collection[str], timeout_s: float) -> Event | None:
        for ev in list(self._events):
            if ev.get("event") in names:
                self._events.remove(ev)
                return ev
        deadline = time.monotonic() + timeout_s
        while True:
            msg = self._read(deadline)
            if msg is None:
                return None
            if "event" not in msg:
                continue
            if msg.get("event") in names:
                return msg
            self._events.append(msg)

    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass


class FakeQmp:
    """A scripted `QmpLike` for unit tests.

    `events` are ready at once; `after_line[n]` become ready once the
    session has seen its `n`-th serial line (`line_seen`). Every command is
    recorded in `commands` as `(name, arguments)`. With `dump` set, `getfd`
    keeps a copy of the fd it is handed and `dump-guest-memory` writes
    `dump` to it, closes it and queues `DUMP_COMPLETED`; without it a dump
    is refused, so a session built on it takes no core.
    """

    def __init__(
        self,
        events: Iterable[Event],
        *,
        after_line: dict[int, list[Event]] | None = None,
        dump: bytes | None = None,
    ) -> None:
        self._ready: deque[Event] = deque(events)
        self._after = dict(after_line or {})
        self.lines = 0
        self.dump = dump
        self.commands: list[tuple[str, dict[str, object] | None]] = []
        self.closed = False
        self._fd: int | None = None

    def line_seen(self) -> None:
        self.lines += 1
        self._ready.extend(self._after.pop(self.lines, []))

    def execute(
        self,
        cmd: str,
        args: dict[str, object] | None = None,
        *,
        fds: Sequence[int] = (),
        timeout_s: float = 10.0,
    ) -> object:
        self.commands.append((cmd, args))
        if cmd == "getfd" and self.dump is not None and fds:
            self._fd = os.dup(fds[0])
        elif cmd == "dump-guest-memory":
            if self.dump is None or self._fd is None:
                raise QmpError("QMP dump-guest-memory: FakeQmp has no dump")
            with os.fdopen(self._fd, "wb") as f:
                f.write(self.dump)
            self._fd = None
            n = len(self.dump)
            self._ready.append(
                {
                    "event": "DUMP_COMPLETED",
                    "data": {"result": {"total": n, "status": "completed", "completed": n}},
                }
            )
        elif cmd == "quit":
            self._ready.append(
                {"event": "SHUTDOWN", "data": {"guest": False, "reason": "host-qmp-quit"}}
            )
        return {}

    def names(self) -> list[str]:
        """The recorded commands' names, in order."""
        return [c for c, _ in self.commands]

    def poll(self) -> list[Event]:
        out = list(self._ready)
        self._ready.clear()
        return out

    def wait_event(self, names: Collection[str], timeout_s: float) -> Event | None:
        for ev in list(self._ready):
            if ev.get("event") in names:
                self._ready.remove(ev)
                return ev
        return None

    def close(self) -> None:
        self.closed = True
        if self._fd is not None:
            os.close(self._fd)
            self._fd = None


def load_stream(path: Path) -> tuple[dict[str, object], list[Event]]:
    """A recorded fixture: its header (line 1) and its events, in order."""
    rows: list[dict[str, object]] = []
    for n, raw in enumerate(Path(path).read_text(encoding="utf-8").splitlines(), 1):
        if not raw.strip():
            continue
        row = json.loads(raw)
        if not isinstance(row, dict):
            raise QmpError(f"{path}:{n}: not a JSON object")
        rows.append(row)
    if not rows:
        raise QmpError(f"{path}: empty stream")
    return rows[0], rows[1:]


def socket_path() -> str:
    """A QMP socket path in a fresh private directory, short enough for
    macOS's 104-byte limit; `Session.close` removes the directory."""
    return os.path.join(tempfile.mkdtemp(prefix="vibeos-qmp-"), "qmp.sock")


@dataclass(frozen=True)
class Decision:
    """What the rule wants now: an end (`pass` or `fail`), a `cont`, and for
    a failure whether to stop the guest and take a core first."""

    end: Literal["pass", "fail"] | None = None
    cont: bool = False
    core: bool = False
    stop_first: bool = False
    reason: str = ""

    def __bool__(self) -> bool:
        return self.end is not None or self.cont


NOTHING = Decision()


def _fail(reason: str, *, core: bool = True) -> Decision:
    return Decision(end="fail", core=core, stop_first=core, reason=reason)


class EventRule:
    """DESIGN §8.3's event rule for one run's declaration (the table above).

    `waiting` is true while a failed run that expects no panic waits for
    its core moment (`vibeOS: panic: halted`, `GUEST_PANICKED` or
    `NO_EVENT_CORE_S`), and while a `capture` run waits for its vmcore line
    and reset.
    """

    def __init__(
        self, expect: str, resets: int = 0, *, clock: Callable[[], float] = time.monotonic
    ) -> None:
        if expect not in EXPECTS:
            raise HarnessError(f"expect={expect!r}: not one of {EXPECTS}")
        if resets < 0:
            raise HarnessError(f"resets={resets}: negative")
        self.expect = expect
        self.resets = resets
        self._clock = clock
        self._core_at: float | None = None  # the no-event core's deadline
        self._halted = False
        self._panicked = False
        self._crashloaded = False
        self._vmcore = False
        self._reset_count = 0
        self.ended: Literal["pass", "fail"] | None = None

    @property
    def core_pending(self) -> bool:
        """A run that expects no panic failed on a signature and waits for
        its core moment."""
        return self._core_at is not None

    @property
    def waiting(self) -> bool:
        if self._core_at is not None:
            return True
        return self.ended is None and self.expect == "capture" and self._crashloaded

    def _end(self, d: Decision) -> Decision:
        if d.end is not None:
            self.ended = d.end
        return d

    def on_line(self, line: str, *, panic: bool, halted: bool) -> Decision:
        if halted:
            self._halted = True
        if self._core_at is not None:
            if halted:
                # The dump has ended: the core moment of a no-event failure.
                self._core_at = None
            return NOTHING
        if self.ended is not None:
            return NOTHING
        if self.expect == "none" and panic:
            self._core_at = None if halted else self._clock() + NO_EVENT_CORE_S
            return self._end(_fail(f"panic signature in: {line!r}"))
        if self.expect == "capture" and self._crashloaded and VMCORE_WRITTEN in line:
            self._vmcore = True
        return NOTHING

    def on_event(self, ev: Event) -> Decision:
        name = ev.get("event")
        data = ev.get("data")
        info = data if isinstance(data, dict) else {}
        if name == "GUEST_PANICKED":
            self._panicked = True
            self._core_at = None
        if self.ended is not None:
            return NOTHING
        if name == "GUEST_PANICKED":
            if self.expect == "panic":
                return self._end(Decision(end="pass", reason="GUEST_PANICKED"))
            if self.expect == "reset":
                return Decision(cont=True, reason="GUEST_PANICKED")
            return self._end(_fail(f"GUEST_PANICKED in a run declared expect={self.expect}"))
        if name == "GUEST_CRASHLOADED":
            if self.expect != "capture":
                return self._end(
                    _fail(f"GUEST_CRASHLOADED in a run declared expect={self.expect}")
                )
            self._crashloaded = True
            return NOTHING
        if name == "STOP" and self.expect == "capture" and self._crashloaded:
            return self._end(
                _fail(
                    "QEMU stopped the guest after GUEST_CRASHLOADED: capture runs "
                    "need -action panic=none on this QEMU (ROADMAP §10.7)"
                )
            )
        if name == "RESET" and self.expect == "reset":
            self._reset_count += 1
            if self._reset_count > self.resets:
                return self._end(
                    _fail(f"RESET {self._reset_count}, the run declared resets={self.resets}")
                )
            return NOTHING
        if name == "SHUTDOWN" and self.expect == "capture" and self._crashloaded:
            if info.get("reason") != "guest-reset":
                return self._end(
                    _fail(f"SHUTDOWN {info.get('reason')!r} before the capture's reset", core=False)
                )
            if not self._vmcore:
                return self._end(
                    _fail(f"the capture reset before {VMCORE_WRITTEN.strip()!r}", core=False)
                )
            return self._end(Decision(end="pass", reason="capture reset"))
        return NOTHING

    def on_tick(self) -> Decision:
        if self._core_at is not None and self._clock() >= self._core_at:
            self._core_at = None
        return NOTHING

    def on_timeout(self) -> Decision:
        self._core_at = None
        if self.expect == "panic" and self._halted and not self._panicked:
            reason = f"no GUEST_PANICKED within {NO_EVENT_CORE_S:g} s of {PANIC_DONE!r}"
        elif self.expect == "capture" and self._crashloaded:
            reason = f"timed out waiting for {VMCORE_WRITTEN.strip()!r} and the reset"
        else:
            reason = "timed out"
        return self._end(_fail(reason))

    def on_exit(self) -> Decision:
        """QEMU exited (serial EOF) with no end decided."""
        if self.ended is not None:
            return NOTHING
        if self.expect == "panic" and not self._panicked:
            return self._end(_fail("QEMU exited before GUEST_PANICKED", core=False))
        if self.expect == "capture" and self._crashloaded:
            return self._end(_fail("QEMU exited before the capture's reset", core=False))
        return NOTHING

    def core_on_fail(self) -> bool:
        """Whether a failure the runner finds takes a core: every declared
        end (`panic`, `reset`, `capture`) keeps its failed run's core."""
        return self.expect != "none"


class _Source(Protocol):
    def next_event(self) -> tuple[str, str]: ...

    def set_deadline(self, deadline: float) -> None: ...

    def kill(self) -> None: ...

    def wait(self, timeout: float) -> int | None: ...


class Session:
    """One boot's QMP side: the runners' hook object.

    A runner builds it before QEMU (its `sock` goes into `qemu_argv`), calls
    `start` once QEMU runs, `line` for each serial line and `idle` when the
    source reports `idle`, `settle` with any decision they return, `fail`
    for a failure of its own, `timeout` at its deadline, and `close` last.
    `qmp` replaces the socket (unit tests): then `sock` is None.
    """

    def __init__(
        self,
        cfg: QemuConfig,
        label: str,
        qmp: QmpLike | None = None,
        *,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.cfg = cfg
        self.label = label
        self.rule = EventRule(cfg.expect, cfg.resets, clock=clock)
        self.qmp = qmp
        self.sock: str | None = None if qmp is not None else socket_path()
        self.events: list[Event] = []
        self.core: Path | None = None
        self.ended: Literal["pass", "fail"] | None = None
        self.end_event = ""
        self._pending: deque[Event] = deque()
        self._closed = False

    def start(self) -> None:
        """Connect (a real QEMU started with `-S`), negotiate, and `cont`."""
        if self.qmp is None:
            assert self.sock is not None
            self.qmp = QmpClient.connect(self.sock)
        self.qmp.execute("cont")

    def _next_decision(self) -> Decision:
        if self.qmp is not None:
            try:
                self._pending.extend(self.qmp.poll())
            except QmpError:
                pass
        while self._pending:
            ev = self._pending.popleft()
            self.events.append(ev)
            d = self.rule.on_event(ev)
            if d:
                if d.end is not None:
                    self.end_event = str(ev.get("event", ""))
                return d
        return NOTHING

    def line(self, raw: str, *, panic: bool, halted: bool) -> Decision:
        if isinstance(self.qmp, FakeQmp):
            self.qmp.line_seen()
        d = self.rule.on_line(raw, panic=panic, halted=halted)
        if d:
            return d
        return self._next_decision()

    def idle(self) -> Decision:
        d = self._next_decision()
        if d:
            return d
        return self.rule.on_tick()

    def _drain(self, source: _Source, result: RunResult, window_s: float = DRAIN_S) -> bool:
        """Serial after an event, into `result.lines`, until `window_s` of
        quiet or EOF; False at EOF."""
        source.set_deadline(time.monotonic() + window_s)
        while True:
            kind, line = source.next_event()
            if kind in ("line", "partial"):
                result.lines.append(line)
                source.set_deadline(time.monotonic() + window_s)
            elif kind == "idle":
                continue
            else:
                return kind != "eof"

    def _await_core_moment(self, source: _Source, result: RunResult) -> None:
        """A run that expects no panic saw a signature: read serial until
        `vibeOS: panic: halted`, `GUEST_PANICKED` or `NO_EVENT_CORE_S`."""
        source.set_deadline(time.monotonic() + NO_EVENT_CORE_S)
        while self.rule.core_pending:
            kind, line = source.next_event()
            if kind in ("line", "partial"):
                result.lines.append(line)
                halted = PANIC_DONE in (kernel_text(line) or "")
                self.rule.on_line(line, panic=False, halted=halted)
            elif kind == "idle":
                self._next_decision()
                self.rule.on_tick()
            else:
                return

    def settle(
        self,
        source: _Source,
        result: RunResult,
        decision: Decision,
        argv: Sequence[str],
        *,
        why: str | None = None,
    ) -> None:
        """Act on `decision`: `cont`; a passing end drains serial and returns
        with the guest still paused; a failure waits for its core moment,
        stops the guest, takes the core, quits QEMU and raises."""
        if decision.end is None:
            if decision.cont and self.qmp is not None:
                self.qmp.execute("cont")
            return
        if decision.end == "pass":
            self.ended = "pass"
            result.end = self.end_event or decision.reason
            self._drain(source, result)
            return
        waited = self.rule.core_pending
        if waited:
            self._await_core_moment(source, result)
        elif self.end_event:
            self._drain(source, result)
        if why is None:
            msg = f"{decision.reason}{serial_tail(result.lines)}"
        elif waited:
            msg = f"{why}{serial_tail(result.lines)}"
        else:
            msg = why
        self._finish_fail(source, result, argv, msg, core=decision.core)

    def fail(self, source: _Source, result: RunResult, argv: Sequence[str], msg: str) -> NoReturn:
        """A failure the runner found: a core when the declaration keeps one
        (`EventRule.core_on_fail`), then quit, kill and raise."""
        self.rule.ended = "fail"
        self._finish_fail(source, result, argv, msg, core=self.rule.core_on_fail())

    def timeout(
        self, source: _Source, result: RunResult, argv: Sequence[str], why: str
    ) -> NoReturn:
        """The runner's deadline passed: stop, core, quit, raise `why`."""
        result.timed_out = True
        d = self.rule.on_timeout()
        msg = why if d.reason == "timed out" else f"{d.reason}; {why}"
        self._finish_fail(source, result, argv, msg, core=True)

    def _finish_fail(
        self,
        source: _Source,
        result: RunResult,
        argv: Sequence[str],
        msg: str,
        *,
        core: bool,
    ) -> NoReturn:
        self.ended = "fail"
        note = ""
        alive = source.wait(0.0) is None
        can_dump = not (isinstance(self.qmp, FakeQmp) and self.qmp.dump is None)
        if core and alive and self.qmp is not None and can_dump:
            try:
                self.qmp.execute("stop")
                d = run_dir(self.label)
                save_run_files(d, argv, self.cfg.iso)
                self.core = take_core(self.qmp, d)
                note = f"\n--- guest core: {self.core} ---"
            except (HarnessError, OSError) as e:
                note = f"\n--- guest core not taken: {e} ---"
        self._quit(source)
        raise HarnessError(msg + note)

    def _quit(self, source: _Source) -> None:
        if self.qmp is not None:
            try:
                self.qmp.execute("quit", timeout_s=2.0)
            except QmpError:
                pass
        source.kill()

    def close(self) -> None:
        """Quit a guest a passing end left paused; drop the socket."""
        if self._closed:
            return
        self._closed = True
        if self.qmp is not None:
            if self.ended == "pass":
                try:
                    self.qmp.execute("quit", timeout_s=2.0)
                except QmpError:
                    pass
            self.qmp.close()
        if self.sock is not None:
            shutil.rmtree(os.path.dirname(self.sock), ignore_errors=True)


def take_core(
    qmp: QmpLike,
    run_dir: Path,
    *,
    compress: Sequence[str] | None = None,
    timeout_s: float = 300.0,
) -> Path:
    """Dump guest memory as a physical ELF core into `run_dir / "core.zst"`.

    `os.pipe()`; the compressor (`zstd -q -T0 -f -o <core.zst>`, or
    `compress`, a command that reads the core on stdin and writes the file)
    starts on the read end, which the harness then closes; `getfd` hands
    QEMU the write end, which the harness closes too, so the compressor sees
    EOF when QEMU's dump closes its copy. `dump-guest-memory` runs detached;
    the core is done at `DUMP_COMPLETED` with status `completed` and no
    `error`, and the compressor's exit 0.
    """
    run_dir.mkdir(parents=True, exist_ok=True)
    out = run_dir / "core.zst"
    argv = list(compress) if compress is not None else ["zstd", "-q", "-T0", "-f", "-o", str(out)]
    r, w = os.pipe()
    try:
        proc = subprocess.Popen(
            argv,
            stdin=r,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            close_fds=True,
        )
    except OSError as e:
        os.close(r)
        os.close(w)
        raise QmpError(f"core: cannot run {argv[0]}: {e}") from e
    os.close(r)
    try:
        try:
            qmp.execute("getfd", {"fdname": CORE_FD_NAME}, fds=[w])
        finally:
            os.close(w)
        qmp.execute(
            "dump-guest-memory",
            {"paging": False, "protocol": f"fd:{CORE_FD_NAME}", "detach": True},
        )
        ev = qmp.wait_event(("DUMP_COMPLETED",), timeout_s)
        if ev is None:
            raise QmpError(f"core: no DUMP_COMPLETED within {timeout_s:g} s")
        data = ev.get("data")
        info = data if isinstance(data, dict) else {}
        res = info.get("result")
        status = res.get("status") if isinstance(res, dict) else None
        if info.get("error") or status != "completed":
            raise QmpError(f"core: dump {status}: {info.get('error')}")
        try:
            _, err = proc.communicate(timeout=timeout_s)
        except subprocess.TimeoutExpired as e:
            raise QmpError(f"core: {argv[0]} did not finish within {timeout_s:g} s") from e
        if proc.returncode != 0:
            text = err.decode("utf-8", errors="replace").strip()
            raise QmpError(f"core: {argv[0]} exited {proc.returncode}: {text}")
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        if proc.stderr is not None:
            proc.stderr.close()
    return out


def _safe(s: str) -> str:
    return _SAFE.sub("_", s) or "_"


def run_dir(label: str) -> Path:
    """A new `CORES_DIR/<arch>-<tier>/<seq>-<label>/` for one failed run;
    `<seq>` counts up from 001 within the tier."""
    cur = results.current()
    base = CORES_DIR / f"{_safe(cur.arch)}-{_safe(cur.tier)}"
    base.mkdir(parents=True, exist_ok=True)
    while True:
        seqs = [int(p.name[:3]) for p in base.iterdir() if p.name[:3].isdigit()]
        d = base / f"{max(seqs, default=0) + 1:03d}-{_safe(label)}"
        try:
            d.mkdir()
        except FileExistsError:
            continue
        return d


def kernel_elf_for(iso: str) -> Path:
    """The named ELF behind `iso` (C-BUILD-OUTPUTS): `build/vibeos.iso` is
    `build/kernels/vibeos-default.elf`, `build/vibeos-<v>.iso` `…/vibeos-<v>.elf`."""
    p = Path(iso)
    stem = p.name[: -len(".iso")] if p.name.endswith(".iso") else p.name
    variant = stem[len("vibeos-") :] if stem.startswith("vibeos-") else "default"
    return p.parent / "kernels" / f"vibeos-{variant}.elf"


def save_run_files(run_dir: Path, argv: Sequence[str], iso: str) -> None:
    """`qemu-argv.txt` (one shell-quoted line) and `kernel.elf`, the ELF
    behind `iso`, or `kernel.elf.missing` naming where it was looked for."""
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "qemu-argv.txt").write_text(shlex.join(argv) + "\n", encoding="utf-8")
    elf = kernel_elf_for(iso)
    if elf.is_file():
        shutil.copyfile(elf, run_dir / "kernel.elf")
    else:
        (run_dir / "kernel.elf.missing").write_text(f"{elf}\n", encoding="utf-8")


def _head(core_zst: Path, head_bytes: int) -> bytes:
    """The first `head_bytes` of the decompressed core (`zstd -dc`)."""
    try:
        proc = subprocess.Popen(
            ["zstd", "-dc", "--", str(core_zst)],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
    except OSError as e:
        raise QmpError(f"core: cannot run zstd: {e}") from e
    assert proc.stdout is not None
    buf = bytearray()
    try:
        while len(buf) < head_bytes:
            chunk = proc.stdout.read(min(65536, head_bytes - len(buf)))
            if not chunk:
                break
            buf.extend(chunk)
    finally:
        proc.stdout.close()
        proc.kill()
        proc.wait()
    return bytes(buf)


def parse_notes(head: bytes, *, machine: int = EM_X86_64) -> list[tuple[str, int, bytes]]:
    """Every note in an ELF64 little-endian core's `PT_NOTE` segments that
    lies within `head`, as (name, type, descriptor)."""
    if len(head) < 64 or head[:4] != b"\x7fELF":
        raise QmpError("core: not an ELF file")
    if head[4] != 2 or head[5] != 1:
        raise QmpError(f"core: ELF class {head[4]} data {head[5]}, want ELF64 little-endian")
    (e_machine,) = struct.unpack_from("<H", head, 18)
    if e_machine != machine:
        raise QmpError(f"core: e_machine {e_machine}, want {machine}")
    (e_phoff,) = struct.unpack_from("<Q", head, 0x20)
    e_phentsize, e_phnum = struct.unpack_from("<HH", head, 0x36)
    notes: list[tuple[str, int, bytes]] = []
    for i in range(e_phnum):
        off = e_phoff + i * e_phentsize
        if off + 56 > len(head):
            raise QmpError(f"core: program header {i} past the first {len(head)} bytes")
        p_type, _flags, p_offset = struct.unpack_from("<IIQ", head, off)
        (p_filesz,) = struct.unpack_from("<Q", head, off + 32)
        if p_type != 4:  # PT_NOTE
            continue
        pos, end = p_offset, min(p_offset + p_filesz, len(head))
        while pos + 12 <= end:
            namesz, descsz, ntype = struct.unpack_from("<III", head, pos)
            pos += 12
            name = head[pos : pos + namesz].rstrip(b"\0").decode("ascii", errors="replace")
            pos += (namesz + 3) & ~3
            desc = head[pos : pos + descsz]
            pos += (descsz + 3) & ~3
            if pos > end:
                break
            notes.append((name, ntype, bytes(desc)))
    return notes


def core_notes(core_zst: Path, head_bytes: int = 1 << 20) -> list[tuple[str, int, bytes]]:
    """The notes of a `take_core` core, read from its first `head_bytes`
    decompressed bytes: QEMU writes the headers and notes before memory."""
    return parse_notes(_head(core_zst, head_bytes))
