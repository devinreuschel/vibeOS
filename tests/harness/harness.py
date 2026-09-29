"""vibeOS end-to-end serial harness. DESIGN §8.3 / §8.4.

Runs QEMU with serial captured, asserts marker strings appear in order, fails
fast on panic / exception signatures, and quits QEMU the moment the last
expected marker is seen so a green run takes ~2s rather than the full timeout.

Standard library only. `subprocess` with its own timeout, not shell `timeout`,
because macOS coreutils lacks it (ROADMAP §0.6).

One QEMU launcher (`qemu_argv`) and one `VIBEOS_*` reader (`env_config`).
Drivers live in `run_*.py` and must not parse the environment or build argv.

| Variable | Default | Drivers |
|---|---|---|
| `VIBEOS_ISO` | per driver | all |
| `VIBEOS_SMP` | `2` (Makefile `?=`) | all |
| `VIBEOS_QEMU_CPU` | `max` | all |
| `VIBEOS_MEM` | `128M` | all |
| `VIBEOS_BIOS` | unset (SeaBIOS) | all |
| `VIBEOS_QEMU_ACCEL` | `tcg` (empty omits `-accel`) | all |
| `VIBEOS_TIMEOUT` | `60` e2e/ps2, `90` ktest/crash | all |
| `VIBEOS_QEMU_EXTRA` | empty | all |
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
"""

from __future__ import annotations

import os
import random
import re
import select
import shutil
import socket
import subprocess
import tempfile
import time
from collections.abc import Callable, Iterable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import IO

from tests.harness.linesource import LineSource

# Any of these substrings in a serial line means the run has failed. Matches
# exception mnemonics rather than English so shell prose does not false-fire
# (DESIGN §9.7).
PANIC_SIGNATURES: tuple[str, ...] = (
    "panicked at",
    "vibeOS: panic:",
    "#PF",
    "#GP",
    "#UD",
    "#DF",
    "double fault",
    "stack overflow",
)

# End of the dump. expect_panic waits for this so backtrace/logrec are in the log.
PANIC_DONE = "vibeOS: panic: halted"


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
    """

    substring: str
    name: str
    and_contains: tuple[str, ...] = ()

    def matches(self, line: str) -> bool:
        if self.substring not in line:
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


def serial_tail(lines: list[str], n: int = 40) -> str:
    """Last `n` serial lines, for timeout / hang errors."""
    if not lines:
        return " (no serial)"
    tail = lines[-n:]
    body = "\n".join(tail)
    return f"\n--- serial tail {len(tail)}/{len(lines)} ---\n{body}"


def contains_panic(line: str, sigs: tuple[str, ...] = PANIC_SIGNATURES) -> bool:
    return any(s in line for s in sigs)


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
    """Each needle must appear. A tuple means all fragments on one line."""
    lines = list(dump)
    for needle in needles:
        if isinstance(needle, tuple):
            if not any(all(part in line for part in needle) for line in lines):
                raise HarnessError(
                    f"dump missing joint needle {needle!r} in {lines[-8:]!r}"
                )
        elif not any(needle in line for line in lines):
            raise HarnessError(f"dump missing {needle!r} in {lines[-8:]!r}")


def _pick_monitor_path() -> str:
    d = tempfile.mkdtemp(prefix="vibeos-mon-")
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
      ("line", str)   - one line of serial output, without trailing \\n
      ("timeout", "") - the deadline arrived
      ("eof", "")     - the pipe closed
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
    """Small helper: yield lines until deadline / EOF. For tests."""
    reader = DeadlineReader(fd, deadline)
    while True:
        kind, payload = reader.next_event()
        if kind == "line":
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

    def next_event(self) -> tuple[str, str]:
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
        if self._mon is not None:
            try:
                self._mon.close()
            except OSError:
                pass
            self._mon = None
        return code

    def stderr_text(self) -> str:
        return _file_text(self._err)


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
# Keep in sync with Makefile `VIBEOS_* ?=` (`make run`).
DEFAULT_SMP = 2
DEFAULT_CPU = "max"
DEFAULT_MEM = "128M"
DEFAULT_ACCEL = "tcg"
LAPIC_TIMER_MODES = ("tsc-deadline", "periodic", "pit")
CRASH_KILL_MAX_S = 0.05
# `cache=unsafe` drops flushes, so the volatile-cache device never sees them.
NBD_CACHE_MODES = ("writeback", "none", "writethrough")


@dataclass
class QemuConfig:
    iso: str
    smp: int = DEFAULT_SMP
    cpu: str = DEFAULT_CPU
    mem: str = DEFAULT_MEM
    bios: str | None = None  # None = QEMU default (SeaBIOS)
    extra: tuple[str, ...] = ()
    hpet: bool = True
    # None → VIBEOS_QEMU_ACCEL, else DEFAULT_ACCEL. Empty string omits -accel.
    accel: str | None = None
    boot_order: str | None = None
    extra_panic: tuple[str, ...] = ()


@dataclass
class EnvConfig:
    iso: str
    smp: int
    cpu: str
    mem: str
    bios: str | None
    accel: str | None
    timeout: float
    extra: tuple[str, ...]
    tier: str = "adhoc"

    def qemu(
        self,
        *,
        extra: tuple[str, ...] = (),
        hpet: bool = True,
        boot_order: str | None = None,
        extra_panic: tuple[str, ...] = (),
    ) -> QemuConfig:
        return QemuConfig(
            iso=self.iso,
            smp=self.smp,
            cpu=self.cpu,
            mem=self.mem,
            bios=self.bios,
            extra=extra + self.extra,
            hpet=hpet,
            accel=self.accel,
            boot_order=boot_order,
            extra_panic=extra_panic,
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


def env_config(*, default_iso: str, default_timeout: float) -> EnvConfig:
    bios = os.environ.get("VIBEOS_BIOS")
    if bios == "":
        bios = None
    accel_raw = os.environ.get("VIBEOS_QEMU_ACCEL")
    extra = tuple(x for x in os.environ.get("VIBEOS_QEMU_EXTRA", "").split() if x)
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
        bios=bios,
        accel=DEFAULT_ACCEL if accel_raw is None else accel_raw,
        timeout=timeout,
        extra=extra,
        tier=os.environ.get("VIBEOS_TIER") or "adhoc",
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


def virtio_blk_args(
    disk: str,
    smp: int,
    *,
    discard: bool = True,
    nbd: bool = False,
    cache: str = "writeback",
) -> tuple[str, ...]:
    """The virtio-blk drive. With `nbd`, `disk` is the unix socket of the
    volatile-cache device (`nbd-cache`), served with no discard and a
    volatile write cache on the device (`write-cache=on`), so a guest flush
    reaches the server under every `cache` mode."""
    if cache not in NBD_CACHE_MODES:
        raise HarnessError(f"cache={cache!r}: not one of {NBD_CACHE_MODES}")
    device = f"virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues={smp}"
    if nbd:
        drive = (
            f"file.driver=nbd,file.server.type=unix,file.server.path={disk},"
            f"format=raw,if=none,id=vibehd,cache={cache}"
        )
        device += ",write-cache=on"
    else:
        drive = f"file={disk},if=none,id=vibehd,format=raw,cache={cache}"
        if discard:
            drive += ",discard=unmap"
    return ("-drive", drive, "-device", device)


def ktest_devices(disk: str, smp: int) -> tuple[str, ...]:
    return (
        "-device",
        "isa-debug-exit,iobase=0xf4,iosize=0x04",
        "-device",
        "e1000e",
        "-device",
        "edu",
        "-device",
        "virtio-rng-pci,disable-legacy=on",
    ) + virtio_blk_args(disk, smp)


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


def lapic_timer_marker(
    *,
    cpu: str | None = None,
    hpet: bool = True,
    accel: str | None = None,
) -> Marker:
    mode = expected_lapic_mode(cpu=cpu, hpet=hpet, accel=accel)
    return Marker(f"vibeOS: time: lapic_timer ok ({mode})", "lapic_timer_ok")


def _accel_args(cfg: QemuConfig) -> list[str]:
    """`-accel tcg` unless overridden. Empty env/config skips the flag."""
    accel = _accel_name(cfg.accel)
    if accel == "":
        return []
    return ["-accel", accel]


# OVMF BDS PXEs the default e1000 if the CD isn't first/ready. slirp
# answers DHCP; TFTP does not. Silent stall matches VIBEOS_TIMEOUT.
# Hits the UEFI e2e second boot (COM1 is an open pipe; marker boot is not).
OVMF_BOOT_ARGS: tuple[str, ...] = (
    "-boot", "order=d,menu=off",
    "-fw_cfg", "name=opt/org.tianocore/IPv4PXESupport,string=no",
    "-fw_cfg", "name=opt/org.tianocore/IPv6PXESupport,string=no",
    "-fw_cfg", "name=opt/org.tianocore/FirmwareSetupSupport,string=no",
)


def qemu_argv(cfg: QemuConfig, monitor_sock: str | None) -> list[str]:
    argv = [
        "qemu-system-x86_64",
        "-cdrom", cfg.iso,
        "-m", cfg.mem,
        "-smp", str(cfg.smp),
        "-cpu", cfg.cpu,
        "-no-reboot",
        "-display", "none",
        "-serial", "stdio",
    ]
    if monitor_sock is not None:
        argv += ["-monitor", f"unix:{monitor_sock},server=on,wait=off"]
    argv += _accel_args(cfg)
    if not cfg.hpet:
        argv += list(HPET_OFF_MACHINE)
    if cfg.bios:
        argv += ["-bios", cfg.bios]
        argv += list(OVMF_BOOT_ARGS)
    elif cfg.boot_order:
        argv += ["-boot", f"order={cfg.boot_order}"]
    argv += list(cfg.extra)
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

    `expect_panic=True` inverts the panic scanner: exactly one panic
    signature must appear, treated as success. Markers are still checked
    against the lines observed *before* the panic banner.

    `line_source` replaces QEMU (C-LINESOURCE): the PATH and ISO checks are
    skipped and the lines, exit status and stderr come from the source.
    """
    panic_signatures = _panic_sigs(cfg, panic_signatures, extra_panic)
    deadline = time.monotonic() + timeout_s
    src = line_source if line_source is not None else _start_qemu(cfg, deadline)

    result = RunResult()
    marker_idx = 0
    panic_seen: str | None = None
    panic_done = False

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

            if not expect_panic:
                for sig in panic_signatures:
                    if sig in line:
                        result.panic_line = line
                        src.kill()
                        raise HarnessError(
                            f"panic signature {sig!r} in: {line!r}"
                        )
            else:
                for sig in panic_signatures:
                    if sig in line and panic_seen is None:
                        panic_seen = line
                        result.panic_line = line
                if PANIC_DONE in line:
                    panic_done = True

            if marker_idx < len(markers) and markers[marker_idx].matches(line):
                result.matched.append(markers[marker_idx].name)
                marker_idx += 1
                if marker_idx == len(markers) and not expect_panic:
                    # Done. Ask QEMU to exit; kill hard if it drags its feet.
                    src.quit()
                    break

            if expect_panic and panic_seen is not None and marker_idx == len(markers):
                # Wait for the dump trailer so backtrace / logrec are captured.
                if panic_done:
                    src.kill()
                    break
    finally:
        result.exit_code = _reap(src)
        result.stderr = src.stderr_text()

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

    if expect_panic and panic_seen is None:
        raise HarnessError("expected a panic signature; none seen")

    if expect_panic and dump_needles:
        dump = dump_after_panic(result.lines, panic_signatures)
        check_dump_needles(dump, dump_needles)

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
        for line in after
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

    Before the injection any panic signature fails the run. After it the
    lines are collected until `PANIC_DONE`, EOF, or `MCE_DUMP_WAIT_S`, and
    `check_mce_dump` judges them. Never retries.
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
            for sig in panic_signatures:
                if sig in line:
                    result.panic_line = line
                    raise HarnessError(f"panic signature {sig!r} in: {line!r}")
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
            if PANIC_DONE in line:
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
    result.panic_line = next((ln for ln in after if "vibeOS: panic:" in ln), None)
    return result


SERIAL_ECHO_TOKEN = "serial-ok"
PS2_ECHO_TOKEN = "ps2-ok"
SHELL_READY_NEEDLE = "vibeOS: shell ready"


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
    """
    panic_signatures = _panic_sigs(cfg, PANIC_SIGNATURES, ())
    deadline = time.monotonic() + timeout_s
    src = (
        line_source
        if line_source is not None
        else _start_qemu(cfg, deadline, stdin=True)
    )

    result = RunResult()
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
            for sig in panic_signatures:
                if sig in line:
                    result.panic_line = line
                    src.kill()
                    raise HarnessError(
                        f"panic signature {sig!r} in: {line!r}"
                    )
            if not saw_ready and SHELL_READY_NEEDLE in line:
                saw_ready = True
                result.matched.append("shell_ready")
                # Prompt is written without a newline; give the shell
                # thread a beat before stuffing COM1.
                time.sleep(0.2)
                src.send_input(f"echo {SERIAL_ECHO_TOKEN}\n".encode())
                continue
            if saw_ready and not saw_serial and SERIAL_ECHO_TOKEN in line:
                # Line editor reprints the command; wait for the echo
                # payload, not only the typed line.
                if line.strip() == SERIAL_ECHO_TOKEN:
                    saw_serial = True
                    result.matched.append("serial_echo")
                    src.monitor("sendkey " + sendkey_chars(f"echo {PS2_ECHO_TOKEN}\n"))
                    continue
            if saw_serial and not saw_ps2 and line.strip() == PS2_ECHO_TOKEN:
                saw_ps2 = True
                result.matched.append("ps2_echo")
                src.quit()
                break
    finally:
        result.exit_code = _reap(src)
        result.stderr = src.stderr_text()

    report = _qemu_report(result, exited=not result.timed_out)
    if not saw_ready:
        why = f"after {timeout_s}s" if result.timed_out else "before QEMU exited"
        raise HarnessError(
            f"console input: no shell ready {why}; matched={result.matched}{report}"
        )
    if not saw_serial:
        raise HarnessError(f"console input: serial echo missing{report}")
    if not saw_ps2:
        raise HarnessError(f"console input: PS/2 sendkey echo missing (i8042){report}")
    return result


# isa-debug-exit at 0xf4: host status = (value << 1) | 1. DESIGN §8.2.
ISA_DEBUG_PASS = 33  # write 0x10
ISA_DEBUG_FAIL = 35  # write 0x11

KTEST_BEGIN = "vibeOS: ktest: begin"
KTEST_END = "vibeOS: ktest: end"
KTEST_FAIL_PREFIX = "vibeOS: ktest: FAIL"
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
        if kind != "line":
            return
        result.lines.append(line)
        if PANIC_DONE in line:
            return


def check_ktest_output(
    lines: Iterable[str],
    exit_code: int | None,
    *,
    pass_status: int = ISA_DEBUG_PASS,
) -> RunResult:
    """Require begin then end, reject any FAIL line, require pass exit status."""
    result = RunResult()
    result.exit_code = exit_code
    saw_begin = False
    saw_end = False
    fails: list[str] = []
    for line in lines:
        result.lines.append(line)
        for sig in PANIC_SIGNATURES:
            if sig in line:
                result.panic_line = line
                raise HarnessError(f"panic signature {sig!r} in: {line!r}")
        if KTEST_BEGIN in line:
            if saw_end:
                raise HarnessError("ktest begin after end")
            saw_begin = True
        if KTEST_FAIL_PREFIX in line:
            fails.append(line)
        if KTEST_END in line:
            if not saw_begin:
                raise HarnessError("ktest end without begin")
            saw_end = True
    if not saw_begin:
        raise HarnessError("missing marker 'ktest_begin'")
    if not saw_end:
        raise HarnessError("missing marker 'ktest_end'")
    if fails:
        raise HarnessError(f"ktest FAIL: {fails[0]}")
    if exit_code != pass_status:
        raise HarnessError(
            f"isa-debug-exit status {exit_code}, expected {pass_status}"
        )
    return result


def run_qemu_until_exit(
    cfg: QemuConfig,
    timeout_s: float = 60.0,
    panic_signatures: tuple[str, ...] = PANIC_SIGNATURES,
    extra_panic: tuple[str, ...] = (),
    kill_after: Callable[[str], float | None] | None = None,
) -> RunResult:
    """Boot the ISO and wait for QEMU to exit (isa-debug-exit).

    `kill_after(line)` may return seconds-until-SIGKILL. The first non-None
    wins (vibefs crash consistency). A kill is not a harness timeout.
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
    deadline = time.monotonic() + timeout_s
    kill_at: float | None = None
    reader = DeadlineReader(proc.stdout.fileno(), deadline)

    def take(line: str) -> None:
        result.lines.append(line)
        for sig in panic_signatures:
            if sig in line:
                result.panic_line = line
                drain_panic_tail(reader, result)
                proc.kill()
                raise HarnessError(
                    f"panic signature {sig!r} in: {line!r}{serial_tail(result.lines)}"
                )

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
                        if kind != "line":
                            break
                        take(line)
                    break
                result.timed_out = True
                proc.kill()
                break
            if kind == "eof":
                break
            take(line)
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

    if result.timed_out:
        raise HarnessError(
            f"timed out after {timeout_s}s; {len(result.lines)} lines"
            f"{serial_tail(result.lines)}"
        )
    return result


# Boot contract, in order. Extended per DESIGN §8.3 as each phase lands.
# Phase 0 gave us serial + limine; slice A added PMM free-frames; slice B
# `paging: cr3 ok`; slice C `heap ok` / `kva: ready`. Phase 2 slice B
# (ACPI) inserts `paging: mmio uc` after CR3 (only emitted when a real
# LAPIC/IOAPIC/HPET leaf was patched). Phase 2 slice A then emits
# `gdt ok` / `pic: remapped` / `idt ok` after KVA (IST from KVA), then
# `per_cpu: bsp ready` (GS_BASE; DESIGN step 11, after GDT because
# `mov gs` zeros the hidden base), then `acpi: xsdt <n> tables`. Slice C
# adds TSC calibration: a diagnostic `time: calibrated hpet|pit <n>/ms`
# then the exit-gate `time: tsc <n>/ms`. Phase 4 slice A then emits
# `time: lapic_timer ok (<mode>)` after the LAPIC timer is proven (PIC
# masked only then). Phase 3 slice B then emits
# `sched: cpu0 ready` and `irq: enabled` (keyboard stays masked until
# phase 5). Phase 4 slice C then emits `sched: cpu<i> ready` then
# `smp: ap online` for each AP, then `smp: done` before `console ok` /
# `shell ready`. `boot: phase1 done` was retired when `shell ready` became
# the trailing marker (DESIGN §3.3 / §8.3).
# Phase 5 slice B then emits `console ok` after SMP. Phase 6 slice A
# emits `pci: <n> devices` (and per-device lines) after that. Phase 6
# slice C may emit diagnostic `work: ready` / virtio lines (not contract
# markers). Phase 7 slice A emits `block: <name> <n> sectors` for the
# ramdisk, still before `shell ready`. Slice C adds
# `block: <parent>p<N> <n> sectors` for partition children (ram0p1, ram0p2).
# Shell `shell ready` is last. IRQ1 is unmasked
# only after the handler exists. TCG: `VIBEOS_QEMU_ACCEL=tcg` (make default)
# or `VIBEOS_QEMU_EXTRA="-accel tcg"`.
# `pic: remapped` means the PIC step finished (ICW ran, or FADT skip);
# unlike `paging: mmio uc` it is not a claim that ports were programmed.
# Trailing live marker is `shell ready`. Runtime-derived payload uses
# `and_contains`.
_PHASE0_BEFORE_TIME: list[Marker] = [
    Marker("vibeOS: serial online", "serial_online"),
    Marker("vibeOS: limine: rev 3 ok", "limine_ok"),
    Marker(
        "vibeOS: pmm: ",
        "pmm_free_frames",
        and_contains=(" free 4KiB frames",),
    ),
    Marker("vibeOS: paging: cr3 ok", "paging_cr3_ok"),
    Marker("vibeOS: paging: mmio uc", "paging_mmio_uc"),
    Marker("vibeOS: heap ok", "heap_ok"),
    Marker("vibeOS: kva: ready", "kva_ready"),
    Marker("vibeOS: gdt ok", "gdt_ok"),
    Marker("vibeOS: pic: remapped", "pic_remapped"),
    Marker("vibeOS: idt ok", "idt_ok"),
    Marker("vibeOS: per_cpu: bsp ready", "per_cpu_bsp"),
    Marker(
        "vibeOS: acpi: xsdt ",
        "acpi_xsdt",
        and_contains=(" tables",),
    ),
]


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
    after = [
        Marker(
            "vibeOS: time: tsc ",
            "time_tsc",
            and_contains=("/ms",),
        ),
        lapic_timer_marker(cpu=cpu, hpet=hpet, accel=accel),
        Marker("vibeOS: sched: cpu0 ready", "sched_cpu0"),
        Marker("vibeOS: irq: enabled", "irq_enabled"),
    ]
    for i in range(1, max(smp, 1)):
        after.append(
            Marker(f"vibeOS: sched: cpu{i} ready", f"sched_cpu{i}")
        )
        after.append(Marker("vibeOS: smp: ap online", f"smp_ap_online_{i - 1}"))
    after.append(Marker("vibeOS: smp: done", "smp_done"))
    after.append(Marker("vibeOS: console ok", "console_ok"))
    after.append(
        Marker(
            "vibeOS: pci: ",
            "pci_devices",
            and_contains=(" devices",),
        )
    )
    after.append(
        Marker(
            "vibeOS: block: ",
            "block_ramdisk",
            and_contains=(" ram0 ", " sectors"),
        )
    )
    after.append(
        Marker(
            "vibeOS: block: ",
            "block_ram0p1",
            and_contains=(" ram0p1 ", " sectors"),
        )
    )
    after.append(
        Marker(
            "vibeOS: block: ",
            "block_ram0p2",
            and_contains=(" ram0p2 ", " sectors"),
        )
    )
    # gp-test trips after PCI enum / ramdisk and never reaches the shell thread.
    if not gp:
        after.append(Marker("vibeOS: shell ready", "shell_ready"))
    if hpet:
        calib = Marker(
            "vibeOS: time: calibrated hpet ",
            "time_calib_hpet",
            and_contains=("/ms",),
        )
    else:
        calib = Marker(
            "vibeOS: time: calibrated pit ",
            "time_calib_pit",
            and_contains=("/ms",),
        )
    markers = _PHASE0_BEFORE_TIME + [calib] + after
    if gp:
        markers = markers + [
            Marker("vibeOS: boot: gp-test armed", "gp_test_armed"),
        ]
    return markers


PHASE0_MARKERS: list[Marker] = boot_contract_markers()

# Production ISO with HPET emulation off: PIT calib + PIT tick.
PHASE0_PIT_MARKERS: list[Marker] = boot_contract_markers(hpet=False)

# Markers that must appear *before* the deliberate panic in the panic-test
# build. panic-test panics right after the limine handshake, so PMM never
# runs on this path.
PHASE0_PANIC_PREFIX: list[Marker] = [
    Marker("vibeOS: serial online", "serial_online"),
    Marker("vibeOS: limine: rev 3 ok", "limine_ok"),
    Marker("vibeOS: boot: panic-test armed", "panic_test_armed"),
]


def halt_test_markers() -> list[Marker]:
    """Prefix markers for the intentional panic-test ISO."""
    return list(PHASE0_PANIC_PREFIX)


# gp-test boots all the way through IDT, then a deliberate #GP dumps and
# halts. Same expect_panic scanner; full marker contract plus the armed line.
PHASE0_GP_MARKERS: list[Marker] = boot_contract_markers(gp=True)
