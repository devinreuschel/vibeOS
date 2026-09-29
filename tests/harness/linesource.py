"""Line sources for the e2e runners (DESIGN §8.3, C-LINESOURCE).

`run_qemu_and_check` and `run_qemu_console_input` read serial through a
`LineSource`. A real run gets `harness.QemuProcess`, a QEMU child; a unit
test gets `FakeLineSource`, which scripts the serial lines, the exit status
and QEMU's stderr, so the runners are tested without QEMU (ROADMAP §10.2,
F141).

Standard library only, and nothing from `harness.py`, which imports this.
"""

from __future__ import annotations

from collections import deque
from collections.abc import Iterable
from typing import Protocol

# ("line", text) | ("eof", "") | ("timeout", "")
LineEvent = tuple[str, str]


class LineSource(Protocol):
    def next_event(self) -> LineEvent:
        """The next serial line, or `eof` (QEMU exited) or `timeout` (deadline)."""
        ...

    def set_deadline(self, deadline: float) -> None:
        """Move the `time.monotonic()` deadline `next_event` reports `timeout` at."""
        ...

    def send_input(self, data: bytes) -> None:
        """Write `data` to the guest's COM1."""
        ...

    def monitor(self, cmd: str) -> None:
        """Send one HMP command."""
        ...

    def quit(self) -> None:
        """Ask QEMU to quit through the monitor; kill it if it lingers."""
        ...

    def kill(self) -> None: ...

    def wait(self, timeout: float) -> int | None:
        """The exit status, or None if QEMU is still running after `timeout`."""
        ...

    def stderr_text(self) -> str:
        """Everything QEMU wrote to stderr so far."""
        ...


class FakeLineSource:
    """A scripted `LineSource`.

    `next_event` pops `events` in order, then returns `("eof", "")` forever.
    `wait` returns `exit_code`. The runner's calls are recorded in
    `deadlines`, `inputs`, `monitor_cmds`, `quit_sent` and `killed`.
    """

    def __init__(
        self,
        events: Iterable[LineEvent],
        *,
        exit_code: int | None = 0,
        stderr: str = "",
    ) -> None:
        self._events: deque[LineEvent] = deque(events)
        self.exit_code = exit_code
        self.stderr = stderr
        self.deadlines: list[float] = []
        self.inputs: list[bytes] = []
        self.monitor_cmds: list[str] = []
        self.quit_sent = False
        self.killed = False

    @classmethod
    def from_lines(
        cls,
        lines: Iterable[str],
        *,
        end: str = "eof",
        exit_code: int | None = 0,
        stderr: str = "",
    ) -> FakeLineSource:
        """One `line` event per line, then an `end` event (`eof` or `timeout`)."""
        events: list[LineEvent] = [("line", line) for line in lines]
        events.append((end, ""))
        return cls(events, exit_code=exit_code, stderr=stderr)

    def next_event(self) -> LineEvent:
        if self._events:
            return self._events.popleft()
        return ("eof", "")

    def set_deadline(self, deadline: float) -> None:
        self.deadlines.append(deadline)

    def send_input(self, data: bytes) -> None:
        self.inputs.append(data)

    def monitor(self, cmd: str) -> None:
        self.monitor_cmds.append(cmd)

    def quit(self) -> None:
        self.quit_sent = True

    def kill(self) -> None:
        self.killed = True

    def wait(self, timeout: float) -> int | None:
        return self.exit_code

    def stderr_text(self) -> str:
        return self.stderr
