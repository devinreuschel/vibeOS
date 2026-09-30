"""vibeOS end-to-end serial harness. DESIGN §8.3 / §8.4.

Runs QEMU with serial captured, asserts marker strings appear in order, fails
fast on panic / exception signatures, and quits QEMU the moment the last
expected marker is seen so a green run takes ~2s rather than the full timeout.

Standard library only. `subprocess` with its own timeout, not shell `timeout`,
because macOS coreutils lacks it (ROADMAP §0.6).

One QEMU launcher (`qemu_argv`) and one `VIBEOS_*` reader (`env_config`).
Drivers live in `run_*.py` and must not parse the environment or build argv;
`run_interactive.py` is the one behind `make run`, `make run-panic` and
`make debug`.

| Variable | Default | Drivers |
|---|---|---|
| `VIBEOS_ISO` | per driver | all, `run_interactive` |
| `VIBEOS_SMP` | `2` | all, `run_interactive` |
| `VIBEOS_QEMU_CPU` | `max` | all, `run_interactive` |
| `VIBEOS_MEM` | `128M` | all, `run_interactive` |
| `VIBEOS_BIOS` | unset, `seabios`: SeaBIOS; `uefi`: probe, pflash | all, `run_interactive` |
| `VIBEOS_FW_X86_64` | probed (`FIRMWARE_TABLE`) | all, `run_interactive` (`VIBEOS_BIOS=uefi`) |
| `VIBEOS_FW_AARCH64` | probed (`FIRMWARE_TABLE`) | none yet (ROADMAP §11.7) |
| `VIBEOS_QEMU_ACCEL` | `tcg` (empty omits `-accel`) | all, `run_interactive` |
| `VIBEOS_TIMEOUT` | `60` (`BOOT_ALLOWANCE_S`), none interactive | all, `run_interactive` |
| `VIBEOS_QEMU_EXTRA` | empty | all, `run_interactive` |
| `VIBEOS_TIER` | `adhoc`; each `make test-*` recipe sets its target name | all (`results.py`) |
| `VIBEOS_EXPECT_PANIC` | off (`""` / `0`) | `run_e2e` |
| `VIBEOS_GP_TEST` | off | `run_e2e` |
| `VIBEOS_EXPECT_PIT` | off | `run_e2e` |
| `VIBEOS_MCE_TEST` | off | `run_e2e` |
| `VIBEOS_SKIP_PERSIST` | off | `run_ktest` |
| `VIBEOS_CRASH_ROUNDS` | `8` | `run_vibefs_crash` |
| `VIBEOS_CRASH_SEED` | time-based | `run_vibefs_crash` |
| `VIBEOS_MKFS` | `mkfs-vibefs` | `run_vibefs_crash`, `run_e2e` |
| `VIBEOS_FSCK` | `fsck-vibefs` | `run_vibefs_crash` |
| `VIBEOS_NBD_CACHE` | `nbd-cache` | `run_vibefs_crash` |
| `VIBEOS_VIBEFS_CAT` | `vibefs-cat` | `run_vibefs_crash` |
| `VIBEOS_QEMU_VERSION` | unset; the QEMU a CI job pins | all (`qemu_argv`, `CI` on Linux) |
| `VIBEOS_CMDLINE` | empty; fw_cfg command-line words (BOOT.md §3.2) | all (`qemu`) |
| `VIBEOS_KTEST` | empty; `vibeos.ktest=<value>`, no whitespace | all (`qemu`) |
| `VIBEOS_KTEST_REPEAT` | unset; `vibeos.ktest_repeat=<n>`, a decimal; kernel checks 1-1000 | all |

`VIBEOS_TIMEOUT` is the boot allowance (DESIGN §8.2): the whole of an e2e,
ps2 or crash boot, and a ktest boot before `begin` and after `end`. Between
them each run has its printed deadline plus 5 s, and each gap 5 s
(`KtestDeadlines`).
"""

from __future__ import annotations

import atexit
import math
import os
import random
import re
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Iterable, Iterator, Mapping, Sequence
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import IO, NamedTuple

from tests.harness import frame, registry
from tests.harness.frame import kernel_text as kernel_text
from tests.harness.linesource import LineSource

# The rows of the marker registry, `tests/contract/markers.toml` (ROADMAP
# §10.2): the contract lists and the failure lists below are built from them.
ROWS: tuple[registry.Row, ...] = registry.load_rows()

# Any of these substrings in a kernel line (a framed one, DESIGN §2.6) means
# the run has failed: the head signature of each kernel `failure` row. The
# mnemonics match rather than English, so shell prose does not false-fire
# (DESIGN §9.7). A user program's line never matches: every driver also fails
# on `frame.USER_FAILURES` in user text and on `frame.LIMINE_SIGNATURES`
# before the first framed line.
PANIC_SIGNATURES: tuple[str, ...] = registry.signatures(ROWS, {frame.KERNEL})
# The kernel `failure` rows with a placeholder before their last literal, as
# patterns: a kernel line one of them matches fails the run too.
FAILURE_PATTERNS: tuple[re.Pattern[str], ...] = registry.failure_patterns(ROWS, {frame.KERNEL})

# End of the dump. expect_panic waits for this so backtrace/logrec are in the log.
PANIC_DONE = "vibeOS: panic: halted"
# How long QEMU has after PANIC_DONE to exit with PANIC_EXIT_STATUS.
PANIC_EXIT_S = 10.0

# The line that opens a dump, mirroring the headers `src/log/panic.rs` writes:
# `panic` (the bare `PANIC_BANNER`), `begin_dump`'s `panic: reentered`,
# `exception_vec`, and `exception_halt` for each kind `src/arch/x86_64/idt.rs`
# passes it. Anchored at the start, so a `vibeOS: logrec:` replay never counts.
DUMP_BANNER_RE = re.compile(
    r"^vibeOS: (?:panic:(?: reentered)?$"
    r"|exception: vector \d+ rip=0x"
    r"|(?:#UD|nmi|#DB|#GP|#PF|#DF|#MC) rip=0x)"
)


def is_dump_banner(line: str) -> bool:
    text = kernel_text(line)
    return text is not None and DUMP_BANNER_RE.match(text) is not None


SERIAL_ONLINE = "vibeOS: serial online"


class HarnessError(Exception):
    """Raised when a harness invariant is violated (bad ordering, panic, etc)."""


@dataclass
class Marker:
    """One assertion in the boot contract.

    `substring` is the primary needle. `and_contains` holds any additional
    fragments that must ALSO appear on the same line, so a marker whose
    payload is runtime-generated (a decimal count, a hex address) can be
    pinned to its full `vibeOS: <subsystem>: <state>` shape without
    hard-coding the varying part. See DESIGN §2.6 / §8.3.

    `exactly_before=(needle, n)`: exactly `n` lines containing `needle`
    precede this marker, and none follows it.

    `source` is where the line comes from (`frame.KERNEL` or `frame.USER`):
    a kernel marker matches only a framed line, a user program's only an
    unframed one (DESIGN §2.6). Empty means `frame.source_of(substring)`.
    """

    substring: str
    name: str
    and_contains: tuple[str, ...] = ()
    exactly_before: tuple[str, int] | None = None
    source: str = ""

    def matches(self, raw: str) -> bool:
        line = frame.text_for(self.source or frame.source_of(self.substring), raw)
        if line is None or self.substring not in line:
            return False
        return all(needle in line for needle in self.and_contains)


@dataclass
class RunResult:
    lines: list[str] = field(default_factory=list)
    matched: list[str] = field(default_factory=list)
    exit_code: int | None = None
    timed_out: bool = False
    panic_line: str | None = None
    stderr: str = ""
    # `check_ktest_output`: the name of each run line in order, and each
    # skipped test's reason.
    ktest_runs: list[str] = field(default_factory=list)
    ktest_skips: dict[str, str] = field(default_factory=dict)


def serial_tail(lines: list[str], n: int = 40) -> str:
    """Last `n` serial lines, for timeout / hang errors."""
    if not lines:
        return " (no serial)"
    tail = lines[-n:]
    body = "\n".join(tail)
    return f"\n--- serial tail {len(tail)}/{len(lines)} ---\n{body}"


def panic_signature(raw: str, sigs: tuple[str, ...] = PANIC_SIGNATURES) -> str | None:
    """The first of `sigs` in `raw`'s kernel text, else the first of
    `FAILURE_PATTERNS` that matches it (as its pattern), else None."""
    text = kernel_text(raw)
    if text is None:
        return None
    sig = next((s for s in sigs if s in text), None)
    if sig is not None:
        return sig
    return next((p.pattern for p in FAILURE_PATTERNS if p.search(text)), None)


def contains_panic(line: str, sigs: tuple[str, ...] = PANIC_SIGNATURES) -> bool:
    """A kernel line holding one of `sigs`, or one of `FAILURE_PATTERNS` matches."""
    return panic_signature(line, sigs) is not None


def run_failure(
    raw: str, stream: frame.Stream, sigs: tuple[str, ...] = PANIC_SIGNATURES
) -> tuple[str, str] | None:
    """Why `raw` fails a run, as (reason, the line's text), or None: a panic
    signature in kernel text, a `frame.USER_FAILURES` entry in user text, or
    Limine's panic line before the boot's first framed line. Feeds `raw` to
    `stream`."""
    limine = stream.limine_panic(raw)
    _, text = stream.feed(raw)
    if limine:
        return "Limine panic before the kernel", text
    sig = panic_signature(raw, sigs)
    if sig is not None:
        return f"panic signature {sig!r}", text
    fail = frame.user_failure(raw)
    if fail is not None:
        return f"user failure {fail!r}", text
    return None


def dump_after_panic(
    lines: Iterable[str],
    panic_signatures: tuple[str, ...] = PANIC_SIGNATURES,
) -> list[str]:
    """Lines from the first panic signature through the end."""
    out = list(lines)
    for i, line in enumerate(out):
        if contains_panic(line, panic_signatures):
            return out[i:]
    return []


def check_dump_needles(
    dump: Iterable[str],
    needles: tuple[str | tuple[str, ...], ...] = (),
) -> None:
    """Each needle must appear on a kernel line. A tuple means all fragments on one line."""
    lines = frame.kernel_lines(dump)
    for needle in needles:
        if isinstance(needle, tuple):
            if not any(all(part in line for part in needle) for line in lines):
                raise HarnessError(
                    f"dump missing joint needle {needle!r} in {lines[-8:]!r}"
                )
        elif not any(needle in line for line in lines):
            raise HarnessError(f"dump missing {needle!r} in {lines[-8:]!r}")


def _pick_monitor_path() -> str:
    """A monitor socket path in a fresh `vibeos-mon-*` directory, which is
    removed when the harness process exits, whichever driver made it."""
    d = tempfile.mkdtemp(prefix="vibeos-mon-")
    atexit.register(shutil.rmtree, d, ignore_errors=True)
    return os.path.join(d, "monitor.sock")


def _send_monitor_quit(sock_path: str) -> None:
    """Best-effort QEMU monitor `quit`. Errors are swallowed; the timeout
    hammer picks up anything that ignores us."""
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(1.0)
            s.connect(sock_path)
            s.sendall(b"quit\n")
    except OSError:
        pass


def sendkey_chars(s: str) -> str:
    """QEMU `sendkey` chord for lowercase letters, digits, space, minus.

    Window keyboard and monitor sendkey both go through the i8042. This
    is the TCG stand-in for typing in the QEMU window (DESIGN §3.6 / #66).
    """
    parts: list[str] = []
    for c in s:
        if c == " ":
            parts.append("spc")
        elif c == "-":
            parts.append("minus")
        elif c == "\n":
            parts.append("ret")
        elif "a" <= c <= "z" or "0" <= c <= "9":
            parts.append(c)
        else:
            raise HarnessError(f"unsupported sendkey char {c!r}")
    if not parts:
        raise HarnessError("empty sendkey")
    return "-".join(parts)


def _connect_monitor(sock_path: str, timeout: float = 5.0) -> socket.socket:
    deadline = time.monotonic() + timeout
    last: OSError | None = None
    while time.monotonic() < deadline:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(2.0)
            s.connect(sock_path)
            try:
                s.recv(4096)
            except TimeoutError:
                pass
            return s
        except OSError as e:
            last = e
            time.sleep(0.05)
    raise HarnessError(f"monitor connect failed: {last}")


def _monitor_cmd(mon: socket.socket, cmd: str) -> None:
    mon.sendall((cmd + "\n").encode())
    try:
        mon.settimeout(0.5)
        mon.recv(4096)
    except TimeoutError:
        pass


class DeadlineReader:
    """Deadline-aware line reader over a file descriptor.

    The point is that `file.readline()` blocks on the underlying `read(2)`
    with no way to bail on a wall-clock deadline. QEMU's serial pipe stays
    open long after the guest has printed a partial contract and hlt'd, so
    a blocking readline would wedge forever. This class polls the fd with
    `select` and buffers partial reads until a newline arrives, or the
    deadline passes, or the pipe closes.

    Yields `(kind, payload)`:
      ("line", str)    - one line of serial output, without trailing \\n
      ("partial", str) - the deadline arrived with bytes buffered but no
                         newline: the line the guest was writing, which often
                         shows where it stopped; ("timeout", "") follows
      ("timeout", "")  - the deadline arrived
      ("eof", "")      - the pipe closed

    A loop that tests only for `timeout` and `eof` takes a partial line as
    a line.
    """

    def __init__(self, fd: int, deadline: float) -> None:
        self._fd = fd
        self._deadline = deadline
        self._buf = bytearray()
        os.set_blocking(fd, False)

    def set_deadline(self, deadline: float) -> None:
        self._deadline = deadline

    def _pop_line(self) -> str | None:
        i = self._buf.find(b"\n")
        if i < 0:
            return None
        line = bytes(self._buf[:i])
        del self._buf[: i + 1]
        return line.rstrip(b"\r").decode("utf-8", errors="replace")

    def next_event(self) -> tuple[str, str]:
        # Emit a buffered complete line first.
        line = self._pop_line()
        if line is not None:
            return ("line", line)

        while True:
            remaining = self._deadline - time.monotonic()
            if remaining <= 0:
                if self._buf:
                    tail = bytes(self._buf).rstrip(b"\r").decode("utf-8", errors="replace")
                    self._buf.clear()
                    return ("partial", tail)
                return ("timeout", "")

            # Cap select's own wait so the deadline is honored precisely.
            r, _, _ = select.select([self._fd], [], [], min(remaining, 0.5))
            if not r:
                # No data; loop and re-check the deadline.
                continue

            try:
                chunk = os.read(self._fd, 4096)
            except BlockingIOError:
                continue
            if not chunk:
                # Pipe closed. Flush anything trailing without a newline.
                if self._buf:
                    tail = bytes(self._buf).rstrip(b"\r").decode(
                        "utf-8", errors="replace"
                    )
                    self._buf.clear()
                    return ("line", tail)
                return ("eof", "")

            self._buf.extend(chunk)
            line = self._pop_line()
            if line is not None:
                return ("line", line)
            # No complete line yet, keep pumping.


def iter_lines_with_deadline(fd: int, deadline: float) -> Iterator[str]:
    """Small helper: yield lines, and a partial line at the deadline, until
    deadline / EOF. For tests."""
    reader = DeadlineReader(fd, deadline)
    while True:
        kind, payload = reader.next_event()
        if kind in ("line", "partial"):
            yield payload
        else:
            return


class QemuProcess:
    """A `LineSource` over a QEMU child (C-LINESOURCE).

    Serial comes from stdout through `DeadlineReader`, and only serial:
    stderr goes to a temporary file, which cannot fill and stall QEMU the
    way a second pipe can. `stdin=True` gives the guest's COM1 a pipe for
    `send_input`. `monitor_sock` is the HMP socket `monitor` and `quit` use.
    """

    def __init__(
        self,
        argv: list[str],
        deadline: float,
        *,
        monitor_sock: str | None = None,
        stdin: bool = False,
    ) -> None:
        self._err = tempfile.TemporaryFile()
        self._proc = subprocess.Popen(
            argv,
            stdout=subprocess.PIPE,
            stderr=self._err,
            stdin=subprocess.PIPE if stdin else subprocess.DEVNULL,
            bufsize=0,
        )
        assert self._proc.stdout is not None
        self._reader = DeadlineReader(self._proc.stdout.fileno(), deadline)
        self._monitor_sock = monitor_sock
        self._mon: socket.socket | None = None
        self._stderr: str | None = None  # set, and the files closed, at exit

    def next_event(self) -> tuple[str, str]:
        if self._stderr is not None:
            return ("eof", "")
        return self._reader.next_event()

    def set_deadline(self, deadline: float) -> None:
        self._reader.set_deadline(deadline)

    def send_input(self, data: bytes) -> None:
        if self._proc.stdin is None:
            raise HarnessError("QemuProcess: started without stdin")
        self._proc.stdin.write(data)
        self._proc.stdin.flush()

    def monitor(self, cmd: str) -> None:
        if self._monitor_sock is None:
            raise HarnessError("QemuProcess: started without a monitor")
        if self._mon is None:
            self._mon = _connect_monitor(self._monitor_sock)
        _monitor_cmd(self._mon, cmd)

    def quit(self) -> None:
        if self._mon is not None:
            try:
                _monitor_cmd(self._mon, "quit")
            except OSError:
                pass
        elif self._monitor_sock is not None:
            _send_monitor_quit(self._monitor_sock)
        if self.wait(2.0) is None:
            self.kill()

    def kill(self) -> None:
        self._proc.kill()

    def wait(self, timeout: float) -> int | None:
        try:
            code = self._proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            return None
        if self._stderr is None:
            self._stderr = _file_text(self._err)
            for f in (self._mon, self._err, self._proc.stdout, self._proc.stdin):
                if f is not None:
                    try:
                        f.close()
                    except OSError:
                        pass
            self._mon = None
        return code

    def stderr_text(self) -> str:
        return self._stderr if self._stderr is not None else _file_text(self._err)


def _file_text(f: IO[bytes]) -> str:
    """A child's stderr file so far. `pread` leaves the shared offset alone."""
    fd = f.fileno()
    return os.pread(fd, os.fstat(fd).st_size, 0).decode("utf-8", errors="replace")


def _start_qemu(cfg: QemuConfig, deadline: float, *, stdin: bool = False) -> QemuProcess:
    """Check PATH and the ISO, then start QEMU with a monitor socket."""
    if not shutil.which("qemu-system-x86_64"):
        raise HarnessError("qemu-system-x86_64 not on PATH")
    if not os.path.exists(cfg.iso):
        raise HarnessError(f"ISO missing: {cfg.iso}")
    monitor_sock = _pick_monitor_path()
    return QemuProcess(
        qemu_argv(cfg, monitor_sock), deadline, monitor_sock=monitor_sock, stdin=stdin
    )


def _qemu_report(result: RunResult, *, exited: bool) -> str:
    """QEMU's exit status and stderr tail, then the serial tail (F079).

    `exited`: QEMU ended the run itself. After a timeout the harness killed
    it, so only a non-empty stderr is shown.
    """
    out = f"; QEMU exited with status {result.exit_code}" if exited else ""
    err = result.stderr.splitlines()[-20:]
    if exited or err:
        out += "\n--- qemu stderr ---\n" + ("\n".join(err) if err else "(no stderr)")
    return out + serial_tail(result.lines)


def _reap(src: LineSource) -> int | None:
    """The exit status; kill the source if it has not exited within 5 s."""
    code = src.wait(5.0)
    if code is None:
        src.kill()
        code = src.wait(5.0)
    return code


# QEMU 10 dropped `-no-hpet`. `pc,hpet=off` is the machine property on
# 8.x (where -no-hpet is only deprecated) and on 10.x.
HPET_OFF_MACHINE = ("-machine", "pc,hpet=off")
# On every x86_64 boot (DESIGN §8.4): `pvpanic` for the panic path's event
# and `vmcoreinfo` so `dump-guest-memory` copies the kernel's VMCOREINFO
# note into a core (docs/VMCOREINFO.md). Neither is a PCI device.
FORENSICS_DEVICES = ("-device", "pvpanic", "-device", "vmcoreinfo")
# The only defaults of the QEMU settings: the Makefile sets none, and
# `make run` reads them through `run_interactive.py` (ROADMAP §10.2).
DEFAULT_SMP = 2
DEFAULT_CPU = "max"
DEFAULT_MEM = "128M"
DEFAULT_ACCEL = "tcg"
LAPIC_TIMER_MODES = ("tsc-deadline", "periodic", "pit")
# The timeout model (DESIGN §8.2, ROADMAP §10.2). `VIBEOS_TIMEOUT` is a boot
# allowance, `BOOT_ALLOWANCE_S` in every driver: it bounds each stretch of a
# boot in which no test runs. From `begin` to `end` a run gets its printed
# deadline plus `KTEST_RUN_SLACK_S`, and each gap between lines
# `KTEST_GAP_S`, all times `EnvConfig.timeout_scale`, which is
# `TIMEOUT_SCALE` in every tier until ROADMAP §17.6 (`KtestDeadlines`).
TIMEOUT_SCALE = 1.0
BOOT_ALLOWANCE_S = 60.0
KTEST_RUN_SLACK_S = 5.0
KTEST_GAP_S = 5.0
CRASH_KILL_MAX_S = 0.05
# `cache=unsafe` drops flushes, so the volatile-cache device never sees them.
NBD_CACHE_MODES = ("writeback", "none", "writethrough")


@dataclass
class QemuConfig:
    iso: str
    smp: int = DEFAULT_SMP
    cpu: str = DEFAULT_CPU
    mem: str = DEFAULT_MEM
    # UEFI firmware on pflash; None = QEMU's default SeaBIOS.
    firmware: Firmware | None = None
    extra: tuple[str, ...] = ()
    hpet: bool = True
    # None → VIBEOS_QEMU_ACCEL, else DEFAULT_ACCEL. Empty string omits -accel.
    accel: str | None = None
    boot_order: str | None = None
    extra_panic: tuple[str, ...] = ()
    # The QEMU version the CI job pins (VIBEOS_QEMU_VERSION). None skips the
    # check: only a config `env_config` built carries the pin.
    qemu_version: str | None = None
    # Kernel command-line words for fw_cfg `opt/vibeos/cmdline` (BOOT.md §3.2).
    cmdline: str = ""
    # `vibeos.ktest=` for this boot alone: `qemu_argv` appends it last, so it
    # wins over any `vibeos.ktest=` in `cmdline` (C-CMDLINE).
    ktest: str | None = None
    # A display window (`make run`, `make debug`); False adds `-display none`.
    display: bool = False
    # `-s -S`: a gdb stub on tcp::1234 and the CPUs halted until it continues
    # (`make debug`).
    gdb: bool = False


@dataclass
class EnvConfig:
    iso: str
    smp: int
    cpu: str
    mem: str
    firmware: Firmware | None
    accel: str | None
    timeout: float
    extra: tuple[str, ...]
    tier: str = "adhoc"
    qemu_version: str = ""
    ktest: str = ""
    ktest_repeat: int | None = None
    cmdline: str = ""
    # Multiplies every ktest progress deadline (`KtestDeadlines`); no
    # variable sets it until ROADMAP §17.6.
    timeout_scale: float = TIMEOUT_SCALE

    def fw_cfg_cmdline(self, driver_words: str = "") -> str:
        """The fw_cfg command-line string: the driver's words, then
        `VIBEOS_KTEST`, `VIBEOS_KTEST_REPEAT` and `VIBEOS_CMDLINE`, the
        non-empty ones joined by one space (later words win in the kernel)."""
        parts = [
            driver_words.strip(),
            f"vibeos.ktest={self.ktest}" if self.ktest else "",
            f"vibeos.ktest_repeat={self.ktest_repeat}" if self.ktest_repeat is not None else "",
            self.cmdline.strip(),
        ]
        return " ".join(p for p in parts if p)

    def qemu(
        self,
        *,
        extra: tuple[str, ...] = (),
        hpet: bool = True,
        boot_order: str | None = None,
        extra_panic: tuple[str, ...] = (),
        cmdline: str = "",
    ) -> QemuConfig:
        return QemuConfig(
            iso=self.iso,
            smp=self.smp,
            cpu=self.cpu,
            mem=self.mem,
            firmware=self.firmware,
            extra=extra + self.extra,
            hpet=hpet,
            accel=self.accel,
            boot_order=boot_order,
            extra_panic=extra_panic,
            qemu_version=self.qemu_version,
            cmdline=self.fw_cfg_cmdline(cmdline),
        )


def env_flag(name: str) -> bool:
    """True unless unset, empty, or `"0"`."""
    return os.environ.get(name, "") not in ("", "0")


def env_expect_panic() -> bool:
    return env_flag("VIBEOS_EXPECT_PANIC")


def env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    return int(raw)


def env_str(name: str, default: str) -> str:
    return os.environ.get(name, default)


_VARIANT_RE = re.compile(r"[a-z0-9-]+")


def default_iso(variant: str = "default") -> str:
    """The ISO the Makefile builds for `variant` (C-BUILD-OUTPUTS).

    `build/vibeos.iso` for `default`, `build/vibeos-<variant>.iso` otherwise.
    """
    if not _VARIANT_RE.fullmatch(variant):
        raise ValueError(f"default_iso: bad variant {variant!r}")
    if variant == "default":
        return "build/vibeos.iso"
    return f"build/vibeos-{variant}.iso"


def env_firmware(environ: Mapping[str, str]) -> Firmware | None:
    """`VIBEOS_BIOS`: unset, empty or `seabios` is SeaBIOS (None); `uefi` is
    the x86_64 pair `probe_firmware` finds, which `VIBEOS_FW_X86_64` names."""
    bios = environ.get("VIBEOS_BIOS", "")
    if bios in ("", "seabios"):
        return None
    if bios != "uefi":
        raise HarnessError(
            f"VIBEOS_BIOS={bios}: not seabios or uefi; to boot a firmware image, name it "
            "with VIBEOS_FW_X86_64 and set VIBEOS_BIOS=uefi"
        )
    fw = probe_firmware("x86_64", environ)
    if fw is None:
        raise HarnessError(
            "VIBEOS_BIOS=uefi: no x86_64 UEFI firmware installed (apt: ovmf; Homebrew: qemu); "
            "set VIBEOS_FW_X86_64 to a code image"
        )
    return fw


def env_config(*, default_iso: str, default_timeout: float) -> EnvConfig:
    firmware = env_firmware(os.environ)
    accel_raw = os.environ.get("VIBEOS_QEMU_ACCEL")
    extra = tuple(x for x in os.environ.get("VIBEOS_QEMU_EXTRA", "").split() if x)
    ktest = os.environ.get("VIBEOS_KTEST", "")
    if any(c.isspace() for c in ktest):
        raise HarnessError(f"VIBEOS_KTEST={ktest!r}: whitespace is not allowed")
    repeat_raw = os.environ.get("VIBEOS_KTEST_REPEAT", "")
    ktest_repeat: int | None = None
    if repeat_raw != "":
        try:
            ktest_repeat = int(repeat_raw)
        except ValueError:
            ktest_repeat = -1
        # Only the kernel's range (1 to 1000) is checked in the guest, so a
        # bad count fails on its `bad option` line (DESIGN §8.2).
        if ktest_repeat < 0 or not repeat_raw.isdigit():
            raise HarnessError(f"VIBEOS_KTEST_REPEAT={repeat_raw!r}: not a decimal integer")
    timeout_raw = os.environ.get("VIBEOS_TIMEOUT")
    if timeout_raw is None or timeout_raw == "":
        timeout = default_timeout
    else:
        timeout = float(timeout_raw)
    return EnvConfig(
        iso=os.environ.get("VIBEOS_ISO", default_iso),
        smp=env_int("VIBEOS_SMP", DEFAULT_SMP),
        cpu=os.environ.get("VIBEOS_QEMU_CPU", DEFAULT_CPU),
        mem=os.environ.get("VIBEOS_MEM", DEFAULT_MEM),
        firmware=firmware,
        accel=DEFAULT_ACCEL if accel_raw is None else accel_raw,
        timeout=timeout,
        extra=extra,
        tier=os.environ.get("VIBEOS_TIER") or "adhoc",
        qemu_version=os.environ.get("VIBEOS_QEMU_VERSION", ""),
        ktest=ktest,
        ktest_repeat=ktest_repeat,
        cmdline=os.environ.get("VIBEOS_CMDLINE", ""),
        timeout_scale=TIMEOUT_SCALE,
    )


@contextmanager
def overlay_env(mapping: dict[str, str] | None = None, *, clear: bool = False) -> Iterator[None]:
    """Temporarily replace process env. For harness unit tests (C2)."""
    old = os.environ.copy()
    try:
        if clear:
            os.environ.clear()
        if mapping:
            os.environ.update(mapping)
        yield
    finally:
        os.environ.clear()
        os.environ.update(old)


def make_disk(nbytes: int, prefix: str, *, directory: str | None = None) -> str:
    """tempfile + ftruncate, in `directory` when given. Caller unlinks."""
    fd, path = tempfile.mkstemp(prefix=prefix, suffix=".img", dir=directory)
    try:
        os.ftruncate(fd, nbytes)
    finally:
        os.close(fd)
    return path


# The virtio-blk pattern image (`make_pattern_disk`): every byte of sector
# n is `(n & 0xFF) ^ VBLK_PATTERN_XOR`. The kernel test twin is
# `drivers::ktest::PATTERN_XOR`.
VBLK_PATTERN_XOR = 0xA5
# The sector whose reads `write_blkdebug_config` fails. The kernel test twin
# is `drivers::ktest::BAD_SECTOR`.
VBLK_BAD_SECTOR = 4096


def make_pattern_disk(nbytes: int, prefix: str) -> str:
    """A raw image of `nbytes` (a multiple of 512) in the pattern above, so
    LBA 0 is never all zero and no `kernel_tests` GPT stamp writes it.
    Caller unlinks."""
    if nbytes % 512:
        raise HarnessError(f"pattern disk: {nbytes} bytes is not whole sectors")
    fd, path = tempfile.mkstemp(prefix=prefix, suffix=".img")
    try:
        with os.fdopen(fd, "wb") as f:
            for n in range(nbytes // 512):
                f.write(bytes([(n & 0xFF) ^ VBLK_PATTERN_XOR]) * 512)
    except BaseException:
        os.unlink(path)
        raise
    return path


def write_blkdebug_config(sector: int, prefix: str) -> str:
    """A QEMU `blkdebug` config that fails every read of `sector` with EIO.
    `read_aio` fires in the raw format driver, so the drive keeps
    `format=raw` over `blkdebug:`. Caller unlinks."""
    fd, path = tempfile.mkstemp(prefix=prefix, suffix=".conf")
    with os.fdopen(fd, "w") as f:
        f.write(
            "[inject-error]\n"
            'event = "read_aio"\n'
            'iotype = "read"\n'
            'errno = "5"\n'
            f'sector = "{sector}"\n'
            'once = "off"\n'
            'immediately = "off"\n'
        )
    return path


def virtio_blk_args(
    disk: str,
    smp: int,
    *,
    discard: bool = True,
    nbd: bool = False,
    cache: str = "writeback",
    extra: Sequence[str] = (),
    readonly: bool = False,
    blkdebug: str | None = None,
) -> tuple[str, ...]:
    """The virtio-blk drive, then one more drive per `extra` image. With
    `nbd`, `disk` is the unix socket of the volatile-cache device
    (`nbd-cache`), served with no discard and a volatile write cache on the
    device (`write-cache=on`), so a guest flush reaches the server under
    every `cache` mode. Extra disk `k` (from 1) is drive `vibehd<k>`, a raw
    image with the same queues and `discard`; the guest binds the drives in
    this order, as `vda`, `vdb`, …. `readonly` and `blkdebug` (a config from
    `write_blkdebug_config`) apply to the first drive: `readonly` opens it
    `readonly=on` with no discard, so the device offers `F_RO`, and
    `blkdebug` puts it behind QEMU's error injection, reporting each
    injected error to the guest."""
    if cache not in NBD_CACHE_MODES:
        raise HarnessError(f"cache={cache!r}: not one of {NBD_CACHE_MODES}")
    if nbd and (readonly or blkdebug):
        raise HarnessError("readonly and blkdebug need an image, not nbd")
    if blkdebug is not None and ":" in blkdebug + disk:
        raise HarnessError("a blkdebug config or image path may not contain ':'")
    device = f"virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues={smp}"
    if nbd:
        drive = (
            f"file.driver=nbd,file.server.type=unix,file.server.path={disk},"
            f"format=raw,if=none,id=vibehd,cache={cache}"
        )
        device += ",write-cache=on"
    else:
        src = disk if blkdebug is None else f"blkdebug:{blkdebug}:{disk}"
        drive = f"file={src},if=none,id=vibehd,format=raw,cache={cache}"
        if readonly:
            drive += ",readonly=on"
        elif discard:
            drive += ",discard=unmap"
        if blkdebug is not None:
            drive += ",rerror=report,werror=report"
    args: tuple[str, ...] = ("-drive", drive, "-device", device)
    for k, path in enumerate(extra, start=1):
        d = f"file={path},if=none,id=vibehd{k},format=raw,cache={cache}"
        if discard:
            d += ",discard=unmap"
        dev = f"virtio-blk-pci,drive=vibehd{k},disable-legacy=on,num-queues={smp}"
        args += ("-drive", d, "-device", dev)
    return args


def ktest_devices(
    disk: str,
    smp: int,
    *,
    extra_disks: Sequence[str] = (),
    readonly: bool = False,
    blkdebug: str | None = None,
) -> tuple[str, ...]:
    """The in-guest registry's devices: `disk` is `vda`, and each of
    `extra_disks` a further virtio-blk disk after it. A second virtio-rng
    sits at `00:1d.0`, and a virtio-blk whose probe fails at `00:1e.0`.
    `readonly` and `blkdebug` go to `virtio_blk_args` for `vda`."""
    return (
        "-device",
        "isa-debug-exit,iobase=0xf4,iosize=0x04",
        "-device",
        "e1000e",
        "-device",
        "edu",
        "-device",
        "virtio-rng-pci,disable-legacy=on",
        # A second virtio-rng in a high slot, after the first in bus order,
        # which the driver refuses (`dev::ktest::SPARE_RNG_BDF`).
        "-device",
        "virtio-rng-pci,disable-legacy=on,addr=0x1d",
    ) + virtio_blk_args(
        disk, smp, extra=extra_disks, readonly=readonly, blkdebug=blkdebug
    ) + (
        # A virtio-blk function in a high slot whose probe the kernel_tests
        # hook fails after QENABLE (`dev::ktest::PROBE_BLK_BDF`).
        "-blockdev",
        "driver=null-co,node-name=probeblk,size=1048576,read-zeroes=on",
        "-device",
        "virtio-blk-pci,drive=probeblk,disable-legacy=on,addr=0x1e",
    )


def kill_delay(rng: random.Random) -> float:
    """The crash test's SIGKILL jitter after the `vibeOS: vibefs: wr K` line
    it waits for. In `[0, CRASH_KILL_MAX_S]` s."""
    return rng.uniform(0.0, CRASH_KILL_MAX_S)


def _accel_name(accel: str | None) -> str:
    if accel is not None:
        return accel
    return os.environ.get("VIBEOS_QEMU_ACCEL", DEFAULT_ACCEL)


def effective_accel_name(cfg: QemuConfig) -> str:
    accel = _accel_name(cfg.accel)
    for i, arg in enumerate(cfg.extra[:-1]):
        if arg == "-accel":
            accel = cfg.extra[i + 1].split(",", 1)[0]
    return accel


def expected_lapic_mode(
    *,
    cpu: str | None = None,
    hpet: bool = True,
    accel: str | None = None,
) -> str:
    """Mode the kernel must print. TCG on QEMU 8.x cannot set CPUID.01H:ECX[24]
    (`TCG doesn't support requested feature: tsc-deadline`), so the periodic
    path is what CI sees on `-cpu max`. HPET off refuses periodic calib.
    """
    if not hpet:
        return "pit"
    cpu = cpu if cpu is not None else env_str("VIBEOS_QEMU_CPU", DEFAULT_CPU)
    parts = [p.strip() for p in cpu.split(",")]
    accel_s = _accel_name(accel)
    if accel_s == "tcg" or "-tsc-deadline" in parts:
        return "periodic"
    return "tsc-deadline"


def _accel_args(cfg: QemuConfig) -> list[str]:
    """`-accel tcg` unless overridden. Empty env/config skips the flag."""
    accel = _accel_name(cfg.accel)
    if accel == "":
        return []
    return ["-accel", accel]


class FirmwareError(HarnessError):
    """A UEFI firmware image the probe cannot use: a code image without its
    paired variable-store template, or a `VIBEOS_FW_<ARCH>` that names a
    missing file or no code image of its architecture (C-FIRMWARE)."""


@dataclass(frozen=True)
class Firmware:
    """A UEFI firmware pair: the code image, booted read-only from pflash
    unit 0, and the variable-store template each run copies onto unit 1."""

    arch: str
    code: str
    vars_template: str


@dataclass(frozen=True)
class FirmwarePair:
    """One probe row: a code image and its variable-store template, both
    basenames of files in the same directory, and the directories to look in.
    `{homebrew}` in a directory stands for each of `_homebrew_dirs`."""

    code: str
    vars_template: str
    dirs: tuple[str, ...]


HOMEBREW_QEMU = "{homebrew}/share/qemu"

# The probe table (ROADMAP §10.2, I1), rows in probe order per architecture.
# Ubuntu's apt `ovmf` and `qemu-efi-aarch64`; then Homebrew's `qemu`, which
# ships no vars file named for either 64-bit architecture, so its 32-bit
# ones pair with the 64-bit code. Secure-boot builds need SMM and q35, and
# are left out.
FIRMWARE_TABLE: dict[str, tuple[FirmwarePair, ...]] = {
    "x86_64": (
        FirmwarePair("OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd", ("/usr/share/OVMF",)),
        FirmwarePair("edk2-x86_64-code.fd", "edk2-i386-vars.fd", (HOMEBREW_QEMU,)),
    ),
    "aarch64": (
        FirmwarePair("AAVMF_CODE.fd", "AAVMF_VARS.fd", ("/usr/share/AAVMF",)),
        FirmwarePair("edk2-aarch64-code.fd", "edk2-arm-vars.fd", (HOMEBREW_QEMU,)),
    ),
}

# One variable per architecture: a code image, which overrides the probe.
FIRMWARE_VARS: dict[str, str] = {
    "x86_64": "VIBEOS_FW_X86_64",
    "aarch64": "VIBEOS_FW_AARCH64",
}


def _homebrew_dirs(environ: Mapping[str, str]) -> list[str]:
    """Homebrew's prefixes, in probe order: `$HOMEBREW_PREFIX` when set, then
    Apple Silicon's `/opt/homebrew` and Intel's `/usr/local`."""
    out: list[str] = []
    for d in (environ.get("HOMEBREW_PREFIX", ""), "/opt/homebrew", "/usr/local"):
        if d and d.rstrip("/") not in out:
            out.append(d.rstrip("/"))
    return out


def _rooted(root: str, path: str) -> str:
    return os.path.join(root, path.lstrip("/")) if root else path


def firmware_dirs(pair: FirmwarePair, environ: Mapping[str, str], root: str = "") -> list[str]:
    """The directories `pair` is looked for in, in order, under `root`."""
    out: list[str] = []
    for d in pair.dirs:
        if "{homebrew}" in d:
            out += [_rooted(root, d.format(homebrew=h)) for h in _homebrew_dirs(environ)]
        else:
            out.append(_rooted(root, d))
    return out


def _paired(arch: str, code: str, pair: FirmwarePair) -> Firmware:
    vars_template = os.path.join(os.path.dirname(code), pair.vars_template)
    if not os.path.isfile(vars_template):
        raise FirmwareError(
            f"{arch} firmware code {code} has no variable-store template {vars_template}"
        )
    return Firmware(arch, code, vars_template)


def probe_firmware(
    arch: str, environ: Mapping[str, str] | None = None, *, root: str = ""
) -> Firmware | None:
    """The UEFI firmware pair for `arch`, or None when none is installed.

    `VIBEOS_FW_<ARCH>` names a code image of the architecture's rows, whose
    template is the row's in the same directory. Otherwise the first row
    whose code image exists decides; its template missing fails the probe,
    which never falls through to a later row. `root` prefixes every probed
    path (tests)."""
    if arch not in FIRMWARE_TABLE:
        raise FirmwareError(f"no firmware table for {arch!r} (one of {', '.join(FIRMWARE_TABLE)})")
    environ = os.environ if environ is None else environ
    rows = FIRMWARE_TABLE[arch]
    var = FIRMWARE_VARS[arch]
    override = environ.get(var, "")
    if override:
        if not os.path.isfile(override):
            raise FirmwareError(f"{var}={override}: no such file")
        name = os.path.basename(override)
        pair = next((r for r in rows if r.code == name), None)
        if pair is None:
            other = [a for a, rs in FIRMWARE_TABLE.items() if any(r.code == name for r in rs)]
            what = f"{other[0]}'s code image" if other else "not a code image the probe knows"
            raise FirmwareError(
                f"{var}={override}: {what}; {arch} takes one of "
                + ", ".join(r.code for r in rows)
            )
        return _paired(arch, override, pair)
    for pair in rows:
        for d in firmware_dirs(pair, environ, root):
            code = os.path.join(d, pair.code)
            if os.path.isfile(code):
                return _paired(arch, code, pair)
    return None


# The per-process directory of variable-store copies; atexit removes it.
_VARS_DIR: str | None = None


def new_vars_copy(fw: Firmware) -> str:
    """A fresh copy of `fw`'s variable-store template for one QEMU run, so no
    run sees another's variables and the template stays untouched. It is the
    only way to make one; a later reboot that must keep its variables reuses
    the path instead of calling this again."""
    global _VARS_DIR
    if _VARS_DIR is None:
        _VARS_DIR = tempfile.mkdtemp(prefix="vibeos-fw-")
        atexit.register(remove_vars_copies)
    fd, path = tempfile.mkstemp(prefix=f"{fw.arch}-vars-", suffix=".fd", dir=_VARS_DIR)
    os.close(fd)
    shutil.copyfile(fw.vars_template, path)
    return path


def remove_vars_copies() -> None:
    """Remove this process's variable-store copies."""
    global _VARS_DIR
    if _VARS_DIR is not None:
        shutil.rmtree(_VARS_DIR, ignore_errors=True)
        _VARS_DIR = None


def _drive_file(path: str) -> str:
    """`path` as a `-drive file=` value: QEMU splits options at a comma, and
    reads a doubled one as a literal comma."""
    return path.replace(",", ",,")


def pflash_args(fw: Firmware, vars_copy: str) -> list[str]:
    """The firmware code read-only on pflash unit 0 and a variable store on
    unit 1, never `-bios`, which refuses a code image whose size is not a
    multiple of 64 KiB (Homebrew's `edk2-x86_64-code.fd`, ROADMAP §10.2)."""
    return [
        "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={_drive_file(fw.code)}",
        "-drive", f"if=pflash,format=raw,unit=1,file={_drive_file(vars_copy)}",
    ]


# OVMF BDS PXEs the default e1000 if the CD isn't first/ready. slirp
# answers DHCP; TFTP does not. Silent stall matches VIBEOS_TIMEOUT.
# Hits the UEFI e2e second boot (COM1 is an open pipe; marker boot is not).
OVMF_BOOT_ARGS: tuple[str, ...] = (
    "-boot", "order=d,menu=off",
    "-fw_cfg", "name=opt/org.tianocore/IPv4PXESupport,string=no",
    "-fw_cfg", "name=opt/org.tianocore/IPv6PXESupport,string=no",
    "-fw_cfg", "name=opt/org.tianocore/FirmwareSetupSupport,string=no",
)


QEMU_VERSION_RE = re.compile(r"QEMU emulator version (\d+\.\d+\.\d+)")
# binary -> the version its `--version` printed, so each binary runs once.
_QEMU_VERSIONS: dict[str, str] = {}


def _qemu_version_line(binary: str) -> str:
    try:
        out = subprocess.run(
            [binary, "--version"], capture_output=True, text=True, check=False, timeout=10
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        raise HarnessError(f"{binary} --version failed: {e}") from e
    return out.stdout.splitlines()[0] if out.stdout else ""


def ensure_qemu_pinned(
    binary: str,
    pin: str | None,
    *,
    env: Mapping[str, str] = os.environ,
    platform: str = sys.platform,
    version_line: Callable[[str], str] = _qemu_version_line,
) -> None:
    """Fail when a Linux CI job's QEMU is not the version the job pins (ROADMAP §10.1).

    An image update that moves QEMU then fails every boot loudly instead of
    changing what the tiers test. Skipped with no pin (`None`, a config not
    built by `env_config`), outside CI, and off Linux (macOS, dev hosts).
    """
    if pin is None or not env.get("CI") or not platform.startswith("linux"):
        return
    if pin == "":
        raise HarnessError("CI on Linux: VIBEOS_QEMU_VERSION is unset; the job must pin QEMU")
    found = _QEMU_VERSIONS.get(binary)
    if found is None:
        line = version_line(binary)
        m = QEMU_VERSION_RE.search(line)
        found = m.group(1) if m else line.strip()
        _QEMU_VERSIONS[binary] = found
    if found != pin:
        raise HarnessError(
            f"{binary} is QEMU {found or '(no version)'}, but VIBEOS_QEMU_VERSION pins {pin}; "
            "move the pin and DESIGN §8.6 together"
        )


# The fw_cfg file the kernel appends to Limine's command line (BOOT.md §3.2).
FW_CFG_CMDLINE = "opt/vibeos/cmdline"


def fw_cfg_cmdline_words(cfg: QemuConfig) -> str:
    """The command-line string `qemu_argv` hands fw_cfg: `cfg.cmdline`, then
    `vibeos.ktest=<cfg.ktest>` when set."""
    parts = [cfg.cmdline.strip()]
    if cfg.ktest is not None:
        parts.append(f"vibeos.ktest={cfg.ktest}")
    return " ".join(p for p in parts if p)


def qemu_argv(cfg: QemuConfig, monitor_sock: str | None) -> list[str]:
    argv = [
        "qemu-system-x86_64",
        "-cdrom", cfg.iso,
        "-m", cfg.mem,
        "-smp", str(cfg.smp),
        "-cpu", cfg.cpu,
        "-no-reboot",
    ]
    if not cfg.display:
        # QEMU keeps the last `-display`, so there is only ever this one.
        argv += ["-display", "none"]
    argv += ["-serial", "stdio"]
    if monitor_sock is not None:
        argv += ["-monitor", f"unix:{monitor_sock},server=on,wait=off"]
    argv += _accel_args(cfg)
    if not cfg.hpet:
        argv += list(HPET_OFF_MACHINE)
    if cfg.gdb:
        argv += ["-s", "-S"]
    if cfg.firmware is not None:
        argv += pflash_args(cfg.firmware, new_vars_copy(cfg.firmware))
        argv += list(OVMF_BOOT_ARGS)
    elif cfg.boot_order:
        argv += ["-boot", f"order={cfg.boot_order}"]
    words = fw_cfg_cmdline_words(cfg)
    if words:
        # QEMU's option syntax reads `,,` as one comma inside a value.
        argv += ["-fw_cfg", f"name={FW_CFG_CMDLINE},string={words.replace(',', ',,')}"]
    argv += list(FORENSICS_DEVICES)
    argv += list(cfg.extra)
    ensure_qemu_pinned(argv[0], cfg.qemu_version)
    return argv


def _panic_sigs(
    cfg: QemuConfig,
    base: tuple[str, ...],
    extra: tuple[str, ...],
) -> tuple[str, ...]:
    more = cfg.extra_panic + extra
    if not more:
        return base
    return base + more


def run_qemu_and_check(
    cfg: QemuConfig,
    markers: list[Marker],
    timeout_s: float = 45.0,
    panic_signatures: tuple[str, ...] = PANIC_SIGNATURES,
    extra_panic: tuple[str, ...] = (),
    expect_panic: bool = False,
    dump_needles: tuple[str | tuple[str, ...], ...] = (),
    line_source: LineSource | None = None,
) -> RunResult:
    """Boot the ISO, stream serial, and assert the boot contract.

    On success (all markers seen in order), issues `quit` through the QEMU
    monitor so the process exits quickly.

    `expect_panic=True`: the dump starts at the first line with a panic
    signature or a dump banner. Markers and `exactly_before` counts see only
    the lines before it, so the dump's logrec replay cannot satisfy one. The
    dump must hold exactly one banner and reach `PANIC_DONE`; QEMU must then
    exit within `PANIC_EXIT_S` with `PANIC_EXIT_STATUS`; then `dump_needles`.

    `line_source` replaces QEMU (C-LINESOURCE): the PATH and ISO checks are
    skipped and the lines, exit status and stderr come from the source.

    Panic signatures, the dump and `PANIC_DONE` match only kernel lines
    (framed, DESIGN §2.6); each marker matches its own source's lines.
    Limine's panic line before the first framed line, and a
    `frame.USER_FAILURES` line before the dump, fail the run.
    """
    panic_signatures = _panic_sigs(cfg, panic_signatures, extra_panic)
    deadline = time.monotonic() + timeout_s
    src = line_source if line_source is not None else _start_qemu(cfg, deadline)

    result = RunResult()
    stream = frame.Stream()
    marker_idx = 0
    dump_at: int | None = None  # index in result.lines of the dump's first line
    banners = 0
    panic_done = False
    exact = {m.exactly_before[0]: m.exactly_before[1] for m in markers if m.exactly_before}
    counts = dict.fromkeys(exact, 0)

    def fail(msg: str) -> HarnessError:
        src.kill()
        return HarnessError(msg)

    try:
        while True:
            kind, line = src.next_event()
            if kind == "timeout":
                result.timed_out = True
                src.kill()
                break
            if kind == "eof":
                # QEMU closed its stdout (usually because it exited).
                break

            result.lines.append(line)
            limine = stream.limine_panic(line)
            framed, text = stream.feed(line)
            if limine:
                result.panic_line = line
                raise fail(f"Limine panic before the kernel in: {text!r}")
            ktext = text if framed else None
            sig = panic_signature(line, panic_signatures)
            ufail = frame.user_failure(line)
            if dump_at is None and ufail is not None:
                result.panic_line = line
                raise fail(f"user failure {ufail!r} in: {text!r}")

            if dump_at is None and sig is not None and not expect_panic:
                result.panic_line = line
                raise fail(f"panic signature {sig!r} in: {text!r}")
            if dump_at is None and expect_panic and (sig is not None or is_dump_banner(line)):
                dump_at = len(result.lines) - 1
                result.panic_line = line
                if marker_idx < len(markers):
                    raise fail(
                        f"missing marker {markers[marker_idx].name!r} before the first "
                        f"panic signature: {text!r}{serial_tail(result.lines)}"
                    )

            if dump_at is not None:
                if is_dump_banner(line):
                    banners += 1
                    if banners > 1:
                        raise fail(f"expected one dump banner, saw {banners}: {line!r}")
                if ktext is not None and PANIC_DONE in ktext and not panic_done:
                    panic_done = True
                    src.set_deadline(time.monotonic() + PANIC_EXIT_S)
                continue

            for needle, n in exact.items():
                ntext = frame.text_for(frame.source_of(needle), line)
                if ntext is not None and needle in ntext:
                    counts[needle] += 1
                    if counts[needle] > n:
                        raise fail(
                            f"extra {needle!r} line ({counts[needle]} seen, "
                            f"expected exactly {n}): {line}"
                        )

            if marker_idx < len(markers) and markers[marker_idx].matches(line):
                eb = markers[marker_idx].exactly_before
                if eb is not None and counts[eb[0]] != eb[1]:
                    raise fail(
                        f"{counts[eb[0]]} {eb[0]!r} lines before "
                        f"{markers[marker_idx].name!r}, expected exactly {eb[1]}"
                    )
                result.matched.append(markers[marker_idx].name)
                marker_idx += 1
                if marker_idx == len(markers) and not expect_panic:
                    # Done. Ask QEMU to exit; kill hard if it drags its feet.
                    src.quit()
                    break
    finally:
        result.exit_code = _reap(src)
        result.stderr = src.stderr_text()

    if result.timed_out and panic_done:
        raise HarnessError(
            f"QEMU did not exit within {PANIC_EXIT_S:g} s of {PANIC_DONE!r}"
            f"{_qemu_report(result, exited=False)}"
        )
    if result.timed_out:
        missing = (
            markers[marker_idx].name if marker_idx < len(markers) else "none"
        )
        raise HarnessError(
            f"timed out after {timeout_s}s; {len(result.matched)}/{len(markers)} markers; "
            f"missing {missing!r}{_qemu_report(result, exited=False)}"
        )

    if marker_idx < len(markers):
        missing = markers[marker_idx].name
        raise HarnessError(
            f"missing marker {missing!r} after {len(result.lines)} lines"
            f"{_qemu_report(result, exited=True)}"
        )

    if not expect_panic:
        return result
    if dump_at is None:
        raise HarnessError(
            f"expected a panic signature; none seen{_qemu_report(result, exited=True)}"
        )
    if not panic_done:
        raise HarnessError(
            f"dump ended before {PANIC_DONE!r}{_qemu_report(result, exited=True)}"
        )
    if banners != 1:
        raise HarnessError(f"expected one dump banner, saw {banners}{serial_tail(result.lines)}")
    if result.exit_code != PANIC_EXIT_STATUS:
        raise HarnessError(
            f"panic exit status {result.exit_code}, expected {PANIC_EXIT_STATUS}"
            f"{_qemu_report(result, exited=True)}"
        )
    check_dump_needles(result.lines[dump_at:], dump_needles)
    return result


# Injected machine check (ROADMAP §10.6): QEMU's HMP `mce` operands.
# MCi_STATUS VAL|UC|EN|PCC; MCG_STATUS RIPV|MCIP.
MCE_UC_STATUS = 0xB200_0000_0000_0000
MCE_MCG_STATUS = 0x5
# The `#MC` body's `panic::exception_halt` line, the dump's thread line on
# the CPU the check was injected on, and the end of the dump.
MCE_DUMP_NEEDLES: tuple[str | tuple[str, ...], ...] = (
    "vibeOS: #MC rip=0x",
    ("vibeOS: panic: thread", "cpu=0"),
    PANIC_DONE,
)
# How long the guest has after the injection to finish its dump.
MCE_DUMP_WAIT_S = 20.0
_U64_MAX = (1 << 64) - 1


def mce_monitor_cmd(
    *,
    cpu: int,
    bank: int,
    status: int,
    mcg_status: int,
    addr: int = 0,
    misc: int = 0,
) -> str:
    """The HMP `mce <cpu> <bank> <status> <mcg_status> <addr> <misc>` line."""
    for name, v in (
        ("cpu", cpu),
        ("bank", bank),
        ("status", status),
        ("mcg_status", mcg_status),
        ("addr", addr),
        ("misc", misc),
    ):
        if v < 0 or v > _U64_MAX:
            raise HarnessError(f"mce {name} {v:#x} is not a 64-bit unsigned value")
    return f"mce {cpu} {bank} {status:#x} {mcg_status:#x} {addr:#x} {misc:#x}"


def _monitor_reply(mon: socket.socket, timeout: float) -> str:
    """Read the monitor's text until its next `(qemu)` prompt, EOF, or `timeout`.

    The prompt must follow a newline, so the echoed command line does not end
    the read. The echo (QEMU's readline redraws it once per keystroke) is
    dropped with everything before the first newline, and so are terminal
    escapes; a reply with no newline is returned whole.
    """
    buf = bytearray()
    deadline = time.monotonic() + timeout
    while True:
        text = buf.decode("utf-8", errors="replace")
        nl = text.find("\n")
        if nl >= 0 and "(qemu)" in text[nl:]:
            break
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        try:
            mon.settimeout(min(remaining, 0.5))
            chunk = mon.recv(4096)
        except TimeoutError:
            continue
        except OSError:
            break
        if not chunk:
            break
        buf.extend(chunk)
    text = buf.decode("utf-8", errors="replace")
    nl = text.find("\n")
    if nl >= 0:
        text = text[nl + 1 :]
    text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)
    return " ".join(text.replace("(qemu)", " ").split())


def check_mce_dump(
    after: list[str],
    *,
    exit_code: int | None,
    reply: str,
    needles: tuple[str | tuple[str, ...], ...] = MCE_DUMP_NEEDLES,
) -> None:
    """Check the serial lines seen after an `mce` injection.

    The first needle is the dump's opening line. When it never appeared, the
    error says whether QEMU exited on its own (`exit_code`) and carries the
    monitor's reply, the only place QEMU says why an injection failed.
    """
    if not needles:
        raise HarnessError("check_mce_dump: no needles")
    first = needles[0]
    started = any(
        all(part in line for part in first) if isinstance(first, tuple) else first in line
        for line in frame.kernel_lines(after)
    )
    if not started:
        if exit_code is not None:
            why = f"QEMU exited (status {exit_code}) before the #MC dump"
        else:
            why = f"no #MC dump within {MCE_DUMP_WAIT_S:g}s"
        raise HarnessError(f"{why}; monitor: {reply!r}{serial_tail(after)}")
    check_dump_needles(after, needles)


def run_qemu_inject_mce(
    cfg: QemuConfig,
    markers: list[Marker],
    *,
    cmd: str,
    timeout_s: float,
    dump_needles: tuple[str | tuple[str, ...], ...] = MCE_DUMP_NEEDLES,
) -> RunResult:
    """Boot, wait for `markers`, inject a machine check with `cmd`, check the dump.

    Before the injection a failing line (`run_failure`) fails the run.
    After it the lines are collected until a kernel line holds `PANIC_DONE`,
    EOF, or `MCE_DUMP_WAIT_S`, and `check_mce_dump` judges them. Never
    retries.
    """
    if not shutil.which("qemu-system-x86_64"):
        raise HarnessError("qemu-system-x86_64 not on PATH")
    if not os.path.exists(cfg.iso):
        raise HarnessError(f"ISO missing: {cfg.iso}")

    monitor_sock = _pick_monitor_path()
    argv = qemu_argv(cfg, monitor_sock)
    panic_signatures = _panic_sigs(cfg, PANIC_SIGNATURES, ())

    err = tempfile.TemporaryFile()
    proc = subprocess.Popen(
        argv,
        stdout=subprocess.PIPE,
        stderr=err,
        stdin=subprocess.DEVNULL,
        bufsize=1,
        text=True,
    )
    assert proc.stdout is not None

    result = RunResult()
    stream = frame.Stream()
    marker_idx = 0
    reader = DeadlineReader(proc.stdout.fileno(), time.monotonic() + timeout_s)
    after: list[str] = []
    reply = ""
    exited: int | None = None
    try:
        while marker_idx < len(markers):
            kind, line = reader.next_event()
            if kind == "timeout":
                result.timed_out = True
                missing = markers[marker_idx].name
                raise HarnessError(
                    f"timed out after {timeout_s}s; {len(result.matched)}/{len(markers)} "
                    f"markers; missing {missing!r}{serial_tail(result.lines)}"
                )
            if kind == "eof":
                try:
                    result.exit_code = proc.wait(timeout=5.0)
                except subprocess.TimeoutExpired:
                    result.exit_code = None
                result.stderr = _file_text(err)
                raise HarnessError(
                    f"missing marker {markers[marker_idx].name!r} after "
                    f"{len(result.lines)} lines{_qemu_report(result, exited=True)}"
                )
            result.lines.append(line)
            why = run_failure(line, stream, panic_signatures)
            if why is not None:
                result.panic_line = line
                raise HarnessError(f"{why[0]} in: {why[1]!r}")
            if markers[marker_idx].matches(line):
                result.matched.append(markers[marker_idx].name)
                marker_idx += 1

        with _connect_monitor(monitor_sock) as mon:
            mon.sendall((cmd + "\n").encode())
            reply = _monitor_reply(mon, 2.0)

        reader.set_deadline(time.monotonic() + MCE_DUMP_WAIT_S)
        while True:
            kind, line = reader.next_event()
            if kind == "eof":
                try:
                    exited = proc.wait(timeout=5.0)
                except subprocess.TimeoutExpired:
                    exited = None
                break
            if kind == "timeout":
                break
            result.lines.append(line)
            after.append(line)
            if PANIC_DONE in (kernel_text(line) or ""):
                break
    finally:
        if proc.poll() is None:
            proc.kill()
        try:
            result.exit_code = proc.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            result.exit_code = proc.wait()

    check_mce_dump(after, exit_code=exited, reply=reply, needles=dump_needles)
    result.panic_line = next(
        (ln for ln in after if "vibeOS: panic:" in (kernel_text(ln) or "")), None
    )
    return result


SERIAL_ECHO_TOKEN = "serial-ok"
PS2_ECHO_TOKEN = "ps2-ok"
SHELL_READY_NEEDLE = "vibeOS: shell ready"
# Writeback, deferred reclaim and vibefs commits run on after `shell ready`
# (DESIGN §8.3), so the console boot keeps reading this long past its last reply.
CONSOLE_TAIL_S = 3.0


def _console_tail(
    src: LineSource,
    result: RunResult,
    sigs: tuple[str, ...],
    window_s: float,
    stream: frame.Stream,
) -> None:
    """Read serial for `window_s`: fail on a failing line (`run_failure`) or on QEMU's exit."""
    src.set_deadline(time.monotonic() + window_s)
    while True:
        kind, line = src.next_event()
        if kind == "timeout":
            return
        if kind == "eof":
            result.exit_code = _reap(src)
            result.stderr = src.stderr_text()
            raise HarnessError(
                f"console input: QEMU exited in the {window_s} s after the last reply"
                f"{_qemu_report(result, exited=True)}"
            )
        result.lines.append(line)
        why = run_failure(line, stream, sigs)
        if why is not None:
            result.panic_line = line
            src.kill()
            raise HarnessError(f"{why[0]} in the {window_s} s after the last reply: {why[1]}")


def run_qemu_console_input(
    cfg: QemuConfig,
    timeout_s: float = 45.0,
    *,
    line_source: LineSource | None = None,
) -> RunResult:
    """Boot, then type via COM1 and via PS/2 (`sendkey`). Both must echo.

    `-display none` still has an i8042; QEMU `sendkey` injects set-1
    scancodes on IRQ1, the same path as a focused QEMU window.
    `line_source` replaces QEMU as in `run_qemu_and_check`.

    `shell ready` and the replies are `/bin/sh`'s, so they match only
    unframed lines; panic signatures match only kernel lines (DESIGN §2.6).
    """
    panic_signatures = _panic_sigs(cfg, PANIC_SIGNATURES, ())
    deadline = time.monotonic() + timeout_s
    src = (
        line_source
        if line_source is not None
        else _start_qemu(cfg, deadline, stdin=True)
    )

    result = RunResult()
    stream = frame.Stream()
    saw_ready = False
    saw_serial = False
    saw_ps2 = False

    try:
        while True:
            kind, line = src.next_event()
            if kind == "timeout":
                result.timed_out = True
                src.kill()
                break
            if kind == "eof":
                break
            result.lines.append(line)
            why = run_failure(line, stream, panic_signatures)
            if why is not None:
                result.panic_line = line
                src.kill()
                raise HarnessError(f"{why[0]} in: {why[1]!r}")
            utext = frame.user_text(line)
            if utext is None:
                continue
            if not saw_ready and SHELL_READY_NEEDLE in utext:
                saw_ready = True
                result.matched.append("shell_ready")
                # Prompt is written without a newline; give the shell
                # thread a beat before stuffing COM1.
                time.sleep(0.2)
                src.send_input(f"echo {SERIAL_ECHO_TOKEN}\n".encode())
                continue
            if saw_ready and not saw_serial and SERIAL_ECHO_TOKEN in utext:
                # Line editor reprints the command; wait for the echo
                # payload, not only the typed line.
                if utext.strip() == SERIAL_ECHO_TOKEN:
                    saw_serial = True
                    result.matched.append("serial_echo")
                    src.monitor("sendkey " + sendkey_chars(f"echo {PS2_ECHO_TOKEN}\n"))
                    continue
            if saw_serial and not saw_ps2 and utext.strip() == PS2_ECHO_TOKEN:
                saw_ps2 = True
                result.matched.append("ps2_echo")
                # A later reply step goes before the tail.
                _console_tail(src, result, panic_signatures, CONSOLE_TAIL_S, stream)
                src.quit()
                break
    finally:
        result.exit_code = _reap(src)
        result.stderr = src.stderr_text()

    report = _qemu_report(result, exited=not result.timed_out)
    if not saw_ready:
        when = f"after {timeout_s}s" if result.timed_out else "before QEMU exited"
        raise HarnessError(
            f"console input: no shell ready {when}; matched={result.matched}{report}"
        )
    if not saw_serial:
        raise HarnessError(f"console input: serial echo missing{report}")
    if not saw_ps2:
        raise HarnessError(f"console input: PS/2 sendkey echo missing (i8042){report}")
    return result


# isa-debug-exit at 0xf4: host status = (value << 1) | 1. DESIGN §8.2.
ISA_DEBUG_PASS = 33  # write 0x10
ISA_DEBUG_FAIL = 35  # write 0x11
# The `panic_exit` build writes 0x11 after `panic: halted`.
PANIC_EXIT_STATUS = ISA_DEBUG_FAIL


# ktest protocol (DESIGN §8.2, C-KTEST-PROTO): one kernel line per event.
KTEST_PREFIX = "vibeOS: ktest: "
KTEST_KINDS = ("begin", "run", "ok", "fail", "skip", "info", "end", "bad_option")
_KTEST_NAME = r"[A-Za-z0-9_.-]+"
_KTEST_RUN_RE = re.compile(rf"run ({_KTEST_NAME}) (\d+)")
_KTEST_OK_RE = re.compile(rf"ok ({_KTEST_NAME})(?: \((\d+) us\))?")
_KTEST_FAIL_RE = re.compile(rf"FAIL ({_KTEST_NAME})(?:: (.*))?")
_KTEST_SKIP_RE = re.compile(rf"skip ({_KTEST_NAME})(?:: (.*))?")
_KTEST_INFO_RE = re.compile(rf"info ({_KTEST_NAME}): (.*)")
_KTEST_BAD_RE = re.compile(r"bad option ([^=\s]+)=(.*)")
# The ten slowest runs `print_ktest_summary` lists.
KTEST_SLOWEST = 10


class KtestLine(NamedTuple):
    """One protocol line: `kind` is one of `KTEST_KINDS`."""

    kind: str
    name: str = ""
    # `begin`'s run count; None when the line has none.
    n: int | None = None
    deadline_ms: int | None = None
    us: int | None = None
    # A failure's or skip's reason, an info line's text, `bad option`'s
    # `<key>=<value>`.
    text: str = ""


def parse_ktest_line(raw: str) -> KtestLine | None:
    """The protocol line `raw` holds, or None.

    Only a framed line (C-FRAME) whose text starts with `vibeOS: ktest: `
    and then a protocol word parses, so a `dmesg:` or `logrec:` replay, a
    user program's line, and a `vibeOS: ktest:   <detail>` line never do.
    """
    text = frame.kernel_text(raw)
    if text is None or not text.startswith(KTEST_PREFIX):
        return None
    rest = text[len(KTEST_PREFIX) :].rstrip()
    if rest == "end":
        return KtestLine("end")
    if rest == "begin" or rest.startswith("begin "):
        count = rest[len("begin") :].strip()
        return KtestLine("begin", n=int(count) if count.isdigit() else None)
    m = _KTEST_RUN_RE.fullmatch(rest)
    if m is not None:
        return KtestLine("run", m.group(1), deadline_ms=int(m.group(2)))
    m = _KTEST_OK_RE.fullmatch(rest)
    if m is not None:
        us = m.group(2)
        return KtestLine("ok", m.group(1), us=None if us is None else int(us))
    m = _KTEST_FAIL_RE.fullmatch(rest)
    if m is not None:
        return KtestLine("fail", m.group(1), text=m.group(2) or "")
    m = _KTEST_SKIP_RE.fullmatch(rest)
    if m is not None:
        return KtestLine("skip", m.group(1), text=m.group(2) or "")
    m = _KTEST_INFO_RE.fullmatch(rest)
    if m is not None:
        return KtestLine("info", m.group(1), text=m.group(2))
    m = _KTEST_BAD_RE.fullmatch(rest)
    if m is not None:
        return KtestLine("bad_option", m.group(1), text=f"{m.group(1)}={m.group(2)}")
    return None


def ktest_lines(lines: Iterable[str]) -> list[KtestLine]:
    """The protocol lines among raw serial `lines`, in order."""
    return [k for k in (parse_ktest_line(ln) for ln in lines) if k is not None]


@dataclass
class KtestSummary:
    """What one ktest boot printed."""

    # `begin`'s count; None without a `begin` line or a count.
    begin: int | None = None
    runs: list[KtestLine] = field(default_factory=list)
    oks: list[KtestLine] = field(default_factory=list)
    fails: list[KtestLine] = field(default_factory=list)
    skips: list[KtestLine] = field(default_factory=list)
    infos: list[KtestLine] = field(default_factory=list)

    def slowest(self, k: int = KTEST_SLOWEST) -> list[KtestLine]:
        """The `k` timed `ok` runs that took longest, slowest first."""
        timed = [o for o in self.oks if o.us is not None]
        return sorted(timed, key=lambda o: o.us or 0, reverse=True)[:k]

    def text(self) -> list[str]:
        """The report: passes out of runs, the slowest runs, the info lines."""
        n = self.begin if self.begin is not None else len(self.runs)
        out = [f"[ktest] {len(self.oks)} of {n} runs passed, {len(self.skips)} skipped"]
        for f in self.fails:
            out.append(f"[ktest]   FAIL {f.name}: {f.text}")
        slow = self.slowest()
        if slow:
            out.append(f"[ktest] slowest {len(slow)}:")
            out += [f"[ktest]   {o.us} us {o.name}" for o in slow]
        if self.infos:
            out.append("[ktest] info:")
            out += [f"[ktest]   {i.name}: {i.text}" for i in self.infos]
        return out


def ktest_summary(lines: Iterable[str]) -> KtestSummary:
    """Sort raw serial `lines`' protocol lines into a `KtestSummary`."""
    s = KtestSummary()
    for k in ktest_lines(lines):
        if k.kind == "begin":
            s.begin = k.n
        elif k.kind == "run":
            s.runs.append(k)
        elif k.kind == "ok":
            s.oks.append(k)
        elif k.kind == "fail":
            s.fails.append(k)
        elif k.kind == "skip":
            s.skips.append(k)
        elif k.kind == "info":
            s.infos.append(k)
    return s

# After a panic banner, keep reading this long so the dump's `msg:` line and
# the backtrace land in the HarnessError and the results file.
PANIC_DRAIN_S = 0.4


def drain_panic_tail(
    reader: DeadlineReader, result: RunResult, window_s: float = PANIC_DRAIN_S
) -> None:
    """Read a bit more after the banner so `msg:` is in the HarnessError."""
    reader.set_deadline(time.monotonic() + window_s)
    while True:
        kind, line = reader.next_event()
        if kind == "partial":
            result.lines.append(line)
            continue
        if kind != "line":
            return
        result.lines.append(line)
        if PANIC_DONE in (kernel_text(line) or ""):
            return


def check_ktest_output(
    lines: Iterable[str],
    exit_code: int | None,
    *,
    pass_status: int = ISA_DEBUG_PASS,
) -> RunResult:
    """Require `begin <n>`, exactly `n` runs each with its one result, then
    `end`; reject any FAIL line; require the pass exit status.

    Each line is read through `parse_ktest_line`, so `begin`, `run`, the
    results, `end` and the panic signatures match only kernel lines
    (framed, DESIGN §2.6) at the start of their text: a user program's
    forged `FAIL` line and a `dmesg:` or `logrec:` replay are ignored. A
    `begin` with no count, `begin 0` (no test selected) and a `bad option`
    line fail the run.

    Runs pair by order, since a repeated test reuses its name: each `run
    <name>` opens a run that the next result line (`ok`, `FAIL` or `skip`)
    must close, for the same name. A run left open by the next `run` or by
    `end`, a result with no open run, a result for another name, and a
    count of runs or results other than `begin`'s each fail, naming the
    test. Info lines are never results. The result carries the run names
    (`ktest_runs`) and the skips (`ktest_skips`, name to reason).
    """
    result = RunResult()
    result.exit_code = exit_code
    stream = frame.Stream()
    n: int | None = None
    saw_end = False
    open_run: str | None = None
    results_seen = 0
    fails: list[str] = []
    for raw in lines:
        result.lines.append(raw)
        why = run_failure(raw, stream)
        if why is not None:
            result.panic_line = raw
            raise HarnessError(f"{why[0]} in: {why[1]!r}")
        k = parse_ktest_line(raw)
        if k is None:
            continue
        if k.kind == "bad_option":
            raise HarnessError(f"ktest: bad option {k.text}")
        if k.kind == "begin":
            if saw_end:
                raise HarnessError("ktest begin after end")
            if n is not None:
                raise HarnessError("ktest: a second begin")
            if k.n is None:
                raise HarnessError("ktest begin without a run count")
            if k.n == 0:
                raise HarnessError("ktest: no test selected")
            n = k.n
        elif k.kind == "run":
            if n is None or saw_end:
                raise HarnessError(f"ktest: run {k.name} outside begin and end")
            if open_run is not None:
                raise HarnessError(f"ktest: run {open_run} has no result (next: run {k.name})")
            open_run = k.name
            result.ktest_runs.append(k.name)
        elif k.kind in ("ok", "fail", "skip"):
            if open_run is None:
                raise HarnessError(f"ktest: result for {k.name} with no open run: {k.kind}")
            if k.name != open_run:
                raise HarnessError(f"ktest: run {open_run} got a result for {k.name}")
            open_run = None
            results_seen += 1
            if k.kind == "fail":
                fails.append(frame.kernel_text(raw) or raw)
            elif k.kind == "skip":
                result.ktest_skips[k.name] = k.text
        elif k.kind == "end":
            if n is None:
                raise HarnessError("ktest end without begin")
            if open_run is not None:
                raise HarnessError(f"ktest: run {open_run} has no result before end")
            saw_end = True
    if n is None:
        raise HarnessError("missing marker 'ktest_begin'")
    if not saw_end:
        where = f"; run {open_run} has no result" if open_run is not None else ""
        raise HarnessError(f"missing marker 'ktest_end'{where}")
    if fails:
        raise HarnessError(f"ktest FAIL: {fails[0]}")
    if len(result.ktest_runs) != n or results_seen != n:
        raise HarnessError(
            f"ktest: begin {n}, but {len(result.ktest_runs)} runs and {results_seen} results"
        )
    if exit_code != pass_status:
        raise HarnessError(
            f"isa-debug-exit status {exit_code}, expected {pass_status}"
        )
    return result


class KtestDeadlines:
    """The progress deadline of a ktest boot (DESIGN §8.2, ROADMAP §10.2).

    Each stretch of the boot has its own deadline, set when the stretch
    starts:

    | Stretch | Deadline |
    |---|---|
    | QEMU start to `begin` | `allowance` |
    | a `run` line to its result | `(deadline_ms / 1000 + KTEST_RUN_SLACK_S) * scale` |
    | `begin` or a result to the next `run` or `end` | `KTEST_GAP_S * scale` |
    | `end` to QEMU's exit | `allowance` |

    Only those protocol lines (`parse_ktest_line`) move it; other lines,
    info lines included, extend nothing. It backstops the in-guest deadline,
    which a CPU wedged with IF=0 never checks.
    """

    def __init__(self, allowance: float, scale: float = TIMEOUT_SCALE) -> None:
        self.allowance = allowance
        self.scale = scale
        # "boot" before `begin`, "tests" from `begin` to `end`, "after" then.
        self.stretch = "boot"
        self.last_run: str | None = None
        self.run_open = False
        self.window = allowance
        self.deadline = math.inf

    def start(self, now: float) -> float:
        """QEMU started at `now`: the deadline for `begin`."""
        self.window = self.allowance
        self.deadline = now + self.window
        return self.deadline

    def on_line(self, line: str, now: float) -> float:
        """Read raw serial `line`, seen at `now`: the current deadline."""
        k = parse_ktest_line(line)
        if k is None:
            return self.deadline
        if self.stretch == "boot" and k.kind == "begin":
            self.stretch = "tests"
            self.window = KTEST_GAP_S * self.scale
        elif self.stretch == "tests" and k.kind == "run" and k.deadline_ms is not None:
            self.last_run = k.name
            self.run_open = True
            self.window = (k.deadline_ms / 1000 + KTEST_RUN_SLACK_S) * self.scale
        elif self.stretch == "tests" and k.kind in ("ok", "fail", "skip"):
            self.run_open = False
            self.window = KTEST_GAP_S * self.scale
        elif self.stretch == "tests" and k.kind == "end":
            self.stretch = "after"
            self.window = self.allowance
        else:
            return self.deadline
        self.deadline = now + self.window
        return self.deadline

    def hung_message(self) -> str:
        """What timed out: the last run's name from `begin` to `end`, else
        the stretch."""
        if self.stretch == "boot":
            return f"ktest: no begin within {self.window:g} s of QEMU's start"
        if self.stretch == "after":
            return f"ktest: QEMU did not exit within {self.window:g} s of end"
        if self.last_run is None:
            return f"ktest: no run within {self.window:g} s of begin"
        if self.run_open:
            return f"ktest hung in {self.last_run}: no result within {self.window:g} s"
        return (
            f"ktest hung in {self.last_run}: no run or end within {self.window:g} s "
            "of its result"
        )


def run_qemu_until_exit(
    cfg: QemuConfig,
    timeout_s: float = 60.0,
    panic_signatures: tuple[str, ...] = PANIC_SIGNATURES,
    extra_panic: tuple[str, ...] = (),
    kill_after: Callable[[str], float | None] | None = None,
    *,
    expect_fail: bool = False,
    progress: KtestDeadlines | None = None,
) -> RunResult:
    """Boot the ISO and wait for QEMU to exit (isa-debug-exit).

    Without `progress` the whole boot has `timeout_s`. With it, the ktest
    progress deadline replaces that: `progress` sets the deadline of each
    stretch from the lines it reads, and a timeout names the test of the
    last run line (`KtestDeadlines.hung_message`). A partial line at the
    deadline goes into the serial tail, never to `progress` or the checks.

    `kill_after(line)` may return seconds-until-SIGKILL. The first non-None
    wins (vibefs crash consistency). A kill is not a harness timeout. It gets
    the raw line; a failing line (`run_failure`) ends the run, unless
    `expect_fail` is set: an expect-fail boot (`run_ktest`'s deadline trip)
    reads on through its failure and panic lines and checks them itself.
    """
    if not shutil.which("qemu-system-x86_64"):
        raise HarnessError("qemu-system-x86_64 not on PATH")
    if not os.path.exists(cfg.iso):
        raise HarnessError(f"ISO missing: {cfg.iso}")

    monitor_sock = _pick_monitor_path()
    argv = qemu_argv(cfg, monitor_sock)
    panic_signatures = _panic_sigs(cfg, panic_signatures, extra_panic)

    proc = subprocess.Popen(
        argv,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        bufsize=1,
        text=True,
    )
    assert proc.stdout is not None

    result = RunResult()
    t0 = time.monotonic()
    deadline = progress.start(t0) if progress is not None else t0 + timeout_s
    kill_at: float | None = None
    reader = DeadlineReader(proc.stdout.fileno(), deadline)
    stream = frame.Stream()

    def take(line: str) -> None:
        result.lines.append(line)
        why = run_failure(line, stream, panic_signatures)
        if why is not None and expect_fail:
            if result.panic_line is None:
                result.panic_line = line
        elif why is not None:
            result.panic_line = line
            drain_panic_tail(reader, result)
            proc.kill()
            raise HarnessError(f"{why[0]} in: {why[1]!r}{serial_tail(result.lines)}")

    try:
        while True:
            kind, line = reader.next_event()
            if kind == "timeout":
                now = time.monotonic()
                if kill_at is not None and now < deadline:
                    proc.kill()
                    # The reader checks its deadline before it reads, so lines
                    # already in the pipe at the kill are read here, to EOF.
                    reader.set_deadline(time.monotonic() + 2.0)
                    while True:
                        kind, line = reader.next_event()
                        if kind == "partial":
                            result.lines.append(line)
                            continue
                        if kind != "line":
                            break
                        take(line)
                    break
                result.timed_out = True
                proc.kill()
                break
            if kind == "eof":
                break
            if kind == "partial":
                # Kept for the report; never a line to act on.
                result.lines.append(line)
                continue
            take(line)
            if progress is not None:
                deadline = progress.on_line(line, time.monotonic())
                reader.set_deadline(deadline if kill_at is None else min(deadline, kill_at))
            if kill_after is not None and kill_at is None:
                delay = kill_after(line)
                if delay is not None:
                    kill_at = time.monotonic() + delay
                    reader.set_deadline(min(deadline, kill_at))
    finally:
        try:
            result.exit_code = proc.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            result.exit_code = proc.wait()

    if result.timed_out and progress is not None:
        raise HarnessError(
            f"{progress.hung_message()}; {len(result.lines)} lines{serial_tail(result.lines)}"
        )
    if result.timed_out:
        raise HarnessError(
            f"timed out after {timeout_s}s; {len(result.lines)} lines"
            f"{serial_tail(result.lines)}"
        )
    return result


# The boot contract: the `contract` rows of the registry, in order
# (TESTING.md §8.3 gives the rules). `smp: done`'s count of `ap online`
# lines is the per-AP group's close (`registry.per_ap_close`, F141).
AP_ONLINE = "vibeOS: smp: ap online"


def _marker_for(row: registry.Row, binding: Mapping[str, str]) -> Marker:
    """The `Marker` for a contract row: its bound text split at the placeholders
    left, matched on its source's side of the frame (DESIGN §2.6)."""
    text = registry.bind(row.text, binding)
    frags = [f for f in registry.fragments(text) if f]
    name = registry.bind(row.name or "", binding)
    return Marker(frags[0], name, and_contains=tuple(frags[1:]), source=row.source)


def contract_markers(cfg: registry.BootConfig) -> list[Marker]:
    """`cfg`'s contract as markers, in order."""
    markers: list[Marker] = []
    close = registry.per_ap_close(ROWS, cfg)
    for row, binding in registry.contract(ROWS, cfg):
        m = _marker_for(row, binding)
        if close is not None and row is close[1]:
            m.exactly_before = (registry.head(close[0].text), max(cfg.smp, 1) - 1)
        markers.append(m)
    return markers


def boot_contract_markers(
    *,
    hpet: bool = True,
    cpu: str | None = None,
    accel: str | None = None,
    gp: bool = False,
    smp: int | None = None,
) -> list[Marker]:
    """Live e2e contract. Pins the LAPIC timer mode and SMP AP count."""
    if smp is None:
        smp = env_int("VIBEOS_SMP", DEFAULT_SMP)
    cfg = registry.BootConfig(
        hpet=hpet,
        smp=smp,
        lapic_mode=expected_lapic_mode(cpu=cpu, hpet=hpet, accel=accel),
        gp_test=gp,
    )
    return contract_markers(cfg)


PHASE0_MARKERS: list[Marker] = boot_contract_markers()

# Production ISO with HPET emulation off: PIT calib + PIT tick.
PHASE0_PIT_MARKERS: list[Marker] = boot_contract_markers(hpet=False)


def halt_test_markers() -> list[Marker]:
    """The markers that come before the deliberate panic in the panic-test
    build, which panics right after the Limine handshake: the contract rows
    that hold under `panic_test`."""
    return contract_markers(
        registry.BootConfig(hpet=True, smp=1, lapic_mode="", panic_test=True)
    )


# gp-test boots all the way through IDT, then a deliberate #GP dumps and
# halts. Same expect_panic scanner; full marker contract plus the armed line.
PHASE0_GP_MARKERS: list[Marker] = boot_contract_markers(gp=True)
