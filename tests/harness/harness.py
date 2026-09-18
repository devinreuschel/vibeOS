"""vibeOS end-to-end serial harness. DESIGN §8.3.

Runs QEMU with serial captured, asserts marker strings appear in order, fails
fast on panic / exception signatures, and quits QEMU the moment the last
expected marker is seen so a green run takes ~2s rather than the full timeout.

Standard library only. `subprocess` with its own timeout, not shell `timeout`,
because macOS coreutils lacks it (DESIGN §0.6).
"""

from __future__ import annotations

import os
import select
import shutil
import socket
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from typing import Iterable, Iterator


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


def check_markers_in_order(
    lines: Iterable[str],
    markers: list[Marker],
    panic_signatures: tuple[str, ...] = PANIC_SIGNATURES,
) -> RunResult:
    """Walk `lines` once. Assert `markers` are seen in order.

    Raises `HarnessError` on:
      - a panic signature appearing anywhere
      - the input ending before every marker matched
      - a marker appearing out of the requested order

    Returns the result on success.
    """
    result = RunResult()
    idx = 0
    for line in lines:
        result.lines.append(line)
        for sig in panic_signatures:
            if sig in line:
                result.panic_line = line
                raise HarnessError(f"panic signature {sig!r} in: {line!r}")
        if idx < len(markers) and markers[idx].matches(line):
            result.matched.append(markers[idx].name)
            idx += 1

    if idx < len(markers):
        missing = markers[idx].name
        raise HarnessError(
            f"missing marker {missing!r} after {len(result.lines)} lines"
        )
    return result


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


# QEMU 10 dropped `-no-hpet`. `pc,hpet=off` is the machine property on
# 8.x (where -no-hpet is only deprecated) and on 10.x.
HPET_OFF_MACHINE = ("-machine", "pc,hpet=off")
DEFAULT_ACCEL = "tcg"
LAPIC_TIMER_MODES = ("tsc-deadline", "periodic", "pit")


@dataclass
class QemuConfig:
    iso: str
    smp: int = 2
    cpu: str = "max"
    mem: str = "128M"
    bios: str | None = None  # None = QEMU default (SeaBIOS)
    extra: tuple[str, ...] = ()
    hpet: bool = True
    # None → VIBEOS_QEMU_ACCEL, else "tcg". Empty string omits -accel.
    accel: str | None = None


def _accel_name(accel: str | None) -> str:
    if accel is not None:
        return accel
    return os.environ.get("VIBEOS_QEMU_ACCEL", DEFAULT_ACCEL)


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
    cpu = cpu if cpu is not None else os.environ.get("VIBEOS_QEMU_CPU", "max")
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


def _qemu_argv(cfg: QemuConfig, monitor_sock: str) -> list[str]:
    argv = [
        "qemu-system-x86_64",
        "-cdrom", cfg.iso,
        "-m", cfg.mem,
        "-smp", str(cfg.smp),
        "-cpu", cfg.cpu,
        "-no-reboot",
        "-display", "none",
        "-serial", "stdio",
        "-monitor", f"unix:{monitor_sock},server=on,wait=off",
    ]
    argv += _accel_args(cfg)
    if not cfg.hpet:
        argv += list(HPET_OFF_MACHINE)
    if cfg.bios:
        argv += ["-bios", cfg.bios]
    argv += list(cfg.extra)
    return argv


def run_qemu_and_check(
    cfg: QemuConfig,
    markers: list[Marker],
    timeout_s: float = 45.0,
    panic_signatures: tuple[str, ...] = PANIC_SIGNATURES,
    expect_panic: bool = False,
    dump_needles: tuple[str | tuple[str, ...], ...] = (),
) -> RunResult:
    """Boot the ISO, stream serial, and assert the boot contract.

    On success (all markers seen in order), issues `quit` through the QEMU
    monitor so the process exits quickly.

    `expect_panic=True` inverts the panic scanner: exactly one panic
    signature must appear, treated as success. Markers are still checked
    against the lines observed *before* the panic banner.
    """
    if not shutil.which("qemu-system-x86_64"):
        raise HarnessError("qemu-system-x86_64 not on PATH")
    if not os.path.exists(cfg.iso):
        raise HarnessError(f"ISO missing: {cfg.iso}")

    monitor_sock = _pick_monitor_path()
    argv = _qemu_argv(cfg, monitor_sock)

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
    marker_idx = 0
    deadline = time.monotonic() + timeout_s
    panic_seen: str | None = None
    panic_done = False
    reader = DeadlineReader(proc.stdout.fileno(), deadline)

    try:
        while True:
            kind, line = reader.next_event()
            if kind == "timeout":
                result.timed_out = True
                proc.kill()
                break
            if kind == "eof":
                # QEMU closed its stdout (usually because it exited).
                break

            result.lines.append(line)

            if not expect_panic:
                for sig in panic_signatures:
                    if sig in line:
                        result.panic_line = line
                        proc.kill()
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
                    _send_monitor_quit(monitor_sock)
                    try:
                        proc.wait(timeout=2.0)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                    break

            if expect_panic and panic_seen is not None and marker_idx == len(markers):
                # Wait for the dump trailer so backtrace / logrec are captured.
                if panic_done:
                    proc.kill()
                    break
    finally:
        try:
            result.exit_code = proc.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            result.exit_code = proc.wait()

    if result.timed_out:
        raise HarnessError(
            f"timed out after {timeout_s}s; {len(result.matched)}/{len(markers)} markers"
        )

    if marker_idx < len(markers):
        missing = markers[marker_idx].name
        raise HarnessError(
            f"missing marker {missing!r} after {len(result.lines)} lines"
        )

    if expect_panic and panic_seen is None:
        raise HarnessError("expected a panic signature; none seen")

    if expect_panic and dump_needles:
        dump = dump_after_panic(result.lines, panic_signatures)
        check_dump_needles(dump, dump_needles)

    return result


# isa-debug-exit at 0xf4: host status = (value << 1) | 1. DESIGN §8.2.
ISA_DEBUG_PASS = 33  # write 0x10
ISA_DEBUG_FAIL = 35  # write 0x11

KTEST_BEGIN = "vibeOS: ktest: begin"
KTEST_END = "vibeOS: ktest: end"
KTEST_FAIL_PREFIX = "vibeOS: ktest: FAIL"


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
) -> RunResult:
    """Boot the ISO and wait for QEMU to exit (isa-debug-exit)."""
    if not shutil.which("qemu-system-x86_64"):
        raise HarnessError("qemu-system-x86_64 not on PATH")
    if not os.path.exists(cfg.iso):
        raise HarnessError(f"ISO missing: {cfg.iso}")

    monitor_sock = _pick_monitor_path()
    argv = _qemu_argv(cfg, monitor_sock)

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
    reader = DeadlineReader(proc.stdout.fileno(), deadline)

    try:
        while True:
            kind, line = reader.next_event()
            if kind == "timeout":
                result.timed_out = True
                proc.kill()
                break
            if kind == "eof":
                break
            result.lines.append(line)
            for sig in panic_signatures:
                if sig in line:
                    result.panic_line = line
                    proc.kill()
                    raise HarnessError(f"panic signature {sig!r} in: {line!r}")
    finally:
        try:
            result.exit_code = proc.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            result.exit_code = proc.wait()

    if result.timed_out:
        raise HarnessError(f"timed out after {timeout_s}s; {len(result.lines)} lines")
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
# ramdisk, still before `shell ready`. Shell `shell ready` is last. IRQ1 is unmasked
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
        smp = int(os.environ.get("VIBEOS_SMP", "2"))
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
            and_contains=("ram0", " sectors"),
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

# gp-test boots all the way through IDT, then a deliberate #GP dumps and
# halts. Same expect_panic scanner; full marker contract plus the armed line.
PHASE0_GP_MARKERS: list[Marker] = boot_contract_markers(gp=True)
