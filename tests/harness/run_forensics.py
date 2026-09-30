#!/usr/bin/env python3
"""The forensics tier (`make test-forensics`, ROADMAP §10.7, TESTING.md §8.5).

A `hang_test` build hangs every CPU after `smp: done`: one CPU prints
`vibeOS: hang_test: armed`, holds a spinlock and spins with interrupts off,
and each other CPU spins on that lock. The tier tests the host-side
forensics on real cores, the way the panic e2e tests the panic path:

1. `hang`: the hang ISO at `-smp 4` and 128 MiB, declared `expect=none`
   (C-QMP). The boot contract through `smp: done`, then the armed marker;
   any panic signature or QMP event fails it at once. The run gives up
   `GIVE_UP_S` after the marker, never at the harness timeout: it stops the
   guest, takes a core through `qmp.take_core`'s pipe into
   `build/forensics/hang/`, and quits. `vmcore report --virt --trace` must
   print the report `check_report` wants, the export `check_export` wants
   and a virtual core `check_virt` accepts.
2. `build-id`: the hang core with the `gp` build's ELF exits 3 with
   `vmcore: BUILD-ID mismatch`.
3. `gp`: the `gp_test` ISO, declared `expect=panic`: at `GUEST_PANICKED` it
   takes a core of the paused guest, then quits. The `sig:` message is the
   serial `#GP` line without `vibeOS: `, as `PanicLine` keeps it, with its
   numbers written as `N`; its frames are `gp_test_fault`, `gp_test_trip`
   and `boot_rest`, none of them panic machinery.
4. `highmem`: the hang ISO at `-m 9G`, `-smp 4`: its core goes through the
   pipe (no uncompressed core on disk) and the tool reads it streaming from
   `zstd -dc`; the same `sig:` line as `hang`, and at least 9 GiB of RAM in
   its `core:` line.

Each check is a C-RESULTS `marker` row (`forensics_<case>`), each boot an
`add_boot`. Standard library only.
"""

from __future__ import annotations

import dataclasses
import json
import re
import struct
import sys
import time
from collections.abc import Callable, Sequence
from pathlib import Path

from tests.harness import frame, qmp, registry, results
from tests.harness.harness import (
    BOOT_ALLOWANCE_S,
    PANIC_DONE,
    HarnessError,
    Marker,
    QemuConfig,
    RunResult,
    _reap,
    _start_qemu,
    contract_markers,
    default_iso,
    env_config,
    expected_clocksource,
    expected_lapic_mode,
    panic_signature,
    run_failure,
    run_vmcore,
    serial_tail,
)
from tests.harness.linesource import LineSource

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "build" / "forensics"
# How long a hang runs after its armed marker before the tier takes its
# core: the tier stays inside the §10.1 budget.
GIVE_UP_S = 5.0
HANG_ARMED = "vibeOS: hang_test: armed"
SMP = 4
HIGHMEM = "9G"
HIGHMEM_BYTES = 9 << 30
# `vibeos::log::vmcore::PANIC_LINE_CAP`: the bytes of panic text the core keeps.
PANIC_LINE_CAP = 120
LOG_TAIL = 64
# The frames between the hang's spin and the boot thread's entry, since
# P10-S79 the bootstrap thread's `boot_rest` (TESTING.md §8.3).
HANG_SIG = re.compile(r"^sig: timeout @ \S*hang_test::hold < \S*hang_test::arm < \S*boot_rest$")
WAIT_FRAME = "hang_test::wait"
CORE_LINE = re.compile(r"^core: (\d+) RAM in (\d+) segments$")
CPU_LINE = re.compile(
    r"^cpu (\d+) apic \d+ current (\S+) idle (\S+) runq \[([^\]]*)\] regs (\S+) (.+)$"
)
FRAME_LINE = re.compile(r"^  #(\d+) 0x[0-9a-f]{16} (.+)$")
THREAD_LINE = re.compile(r"^thread (\d+) (ready|running|sleeping|blocked|dead)\b")
LOG_LINE = re.compile(r"^log: last (\d+) of (\d+) \((\d+) dropped\)$")
TRACE_LINE = re.compile(r"^trace: order (global|per-cpu)\b")
STATES = ("ready", "running", "sleeping", "blocked", "dead")
_NUM = re.compile(r"0x[0-9A-Fa-f]+|[0-9]+")

Capture = Callable[[qmp.QmpLike, Path], Path]


def normalize(msg: str) -> str:
    """The signature's message rule (`vmcore::sig::normalize_message`): each
    maximal `0x[0-9A-Fa-f]+` or `[0-9]+` becomes `N`."""
    return _NUM.sub("N", msg)


def panic_line(text: str) -> str:
    """`text` as `PanicLine` keeps it (`vmcore::sig::first_line`): cut at its
    first newline and at `PANIC_LINE_CAP` bytes, back to a character
    boundary, without trailing ASCII whitespace."""
    first = text.split("\n", 1)[0]
    cut = first.encode("utf-8")[:PANIC_LINE_CAP].decode("utf-8", errors="ignore")
    return cut.rstrip(" \t\r\x0b\x0c")


# ------------------------------------------------------------------ boots


@dataclasses.dataclass
class Boot:
    """One boot: its serial, argv, exit status and core."""

    result: RunResult
    argv: list[str]
    core: Path | None = None
    armed_at: float | None = None


def hang_markers(smp: int) -> list[Marker]:
    """The hang build's contract: the boot through `smp: done` and the
    clocksource line, then the armed marker (tests/contract/markers.toml)."""
    cfg = registry.BootConfig(
        hpet=True,
        smp=smp,
        lapic_mode=expected_lapic_mode(hpet=True),
        clocksource=expected_clocksource(hpet=True),
        hang_test=True,
    )
    return contract_markers(cfg)


def gp_markers(smp: int) -> list[Marker]:
    cfg = registry.BootConfig(
        hpet=True,
        smp=smp,
        lapic_mode=expected_lapic_mode(hpet=True),
        clocksource=expected_clocksource(hpet=True),
        gp_test=True,
    )
    return contract_markers(cfg)


def _argv(src: object) -> list[str]:
    argv = getattr(src, "argv", None)
    return list(argv) if isinstance(argv, list) else []


def boot_and_capture(
    cfg: QemuConfig,
    markers: list[Marker],
    out: Path,
    label: str,
    timeout_s: float,
    *,
    line_source: LineSource | None = None,
    qmp_client: qmp.QmpLike | None = None,
    clock: Callable[[], float] = time.monotonic,
    capture: Capture = qmp.take_core,
) -> Boot:
    """Boot `cfg` through `markers` and take a core of the stopped guest.

    `cfg.expect == "none"` (a hang): after the last marker the deadline moves
    to `GIVE_UP_S` later, and reaching it is the core moment. A panic
    signature or a QMP event fails the run at once, with a failure core
    (`qmp.Session`). `cfg.expect == "panic"`: `GUEST_PANICKED` is the core
    moment, the guest paused. Either way the core goes to `out`, then QEMU
    quits. `line_source`, `qmp_client`, `clock` and `capture` replace QEMU,
    its QMP, the time and `qmp.take_core` in unit tests."""
    session = qmp.Session(cfg, label, qmp_client, clock=clock)
    deadline = clock() + timeout_s
    src: LineSource = (
        line_source
        if line_source is not None
        else _start_qemu(cfg, deadline, qmp_sock=session.sock)
    )
    boot = Boot(RunResult(), _argv(src))
    result = boot.result
    stream = frame.Stream()
    idx = 0
    hang = cfg.expect == "none"

    def missing() -> str:
        name = markers[idx].name if idx < len(markers) else "none"
        return f"{len(result.matched)}/{len(markers)} markers; missing {name!r}"

    try:
        session.start()
        while True:
            kind, line = src.next_event()
            if kind == "idle":
                d = session.idle()
                if d:
                    session.settle(src, result, d, boot.argv)
                if session.ended == "pass":
                    break
                continue
            if kind == "timeout":
                if boot.armed_at is not None:
                    break
                session.timeout(
                    src,
                    result,
                    boot.argv,
                    f"{label}: timed out after {timeout_s:g} s; {missing()}"
                    f"{serial_tail(result.lines)}",
                )
            if kind == "eof":
                result.exit_code = _reap(src)
                session.fail(
                    src,
                    result,
                    boot.argv,
                    f"{label}: QEMU exited ({result.exit_code}); {missing()}"
                    f"{serial_tail(result.lines)}",
                )
            result.lines.append(line)
            why = run_failure(line, stream)
            sig = panic_signature(line)
            halted = PANIC_DONE in (frame.kernel_text(line) or "")
            d = session.line(line, panic=sig is not None, halted=halted)
            if d and d.end != "pass":
                session.settle(
                    src, result, d, boot.argv, why=f"{label}: panic signature in: {line!r}"
                )
            if why is not None and hang:
                session.fail(src, result, boot.argv, f"{label}: {why[0]} in: {why[1]!r}")
            if idx < len(markers) and markers[idx].matches(line):
                result.matched.append(markers[idx].name)
                idx += 1
                if idx == len(markers) and hang:
                    boot.armed_at = clock()
                    src.set_deadline(boot.armed_at + GIVE_UP_S)
            if d.end == "pass":
                session.settle(src, result, d, boot.argv)
                break
        if idx < len(markers):
            session.fail(src, result, boot.argv, f"{label}: {missing()}{serial_tail(result.lines)}")
        assert session.qmp is not None
        if hang:
            session.qmp.execute("stop")
        out.mkdir(parents=True, exist_ok=True)
        boot.core = capture(session.qmp, out)
        qmp.save_run_files(out, boot.argv, cfg.iso)
        session.ended = "pass"
        return boot
    finally:
        session.close()
        if result.exit_code is None:
            result.exit_code = _reap(src)
            result.stderr = src.stderr_text()


# ----------------------------------------------------------------- checks


def _lines(report: str) -> list[str]:
    return report.splitlines()


def check_report(report: str, smp: int) -> None:
    """The hang core's report: the signature, a `cpu N` block for each CPU
    with a symbolized frame (CPUs 1 up in `hang_test::wait`), each CPU's
    current thread and run queue, one line per TCB with its state, and the
    last min(64, ring length) log records, the armed marker last."""
    lines = _lines(report)
    if not lines or not HANG_SIG.match(lines[0]):
        raise HarnessError(f"report: line 1 {lines[:1]!r}, want {HANG_SIG.pattern}")
    threads = {m.group(1): m.group(2) for m in map(THREAD_LINE.match, lines) if m}
    if not threads:
        raise HarnessError("report: no thread lines")
    bad = [ln for ln in lines if ln.startswith("thread ") and not THREAD_LINE.match(ln)]
    if bad:
        raise HarnessError(f"report: thread line without a state: {bad[0]!r}")
    seen: set[int] = set()
    for i, ln in enumerate(lines):
        m = CPU_LINE.match(ln)
        if m is None:
            continue
        cpu = int(m.group(1))
        seen.add(cpu)
        cur = m.group(2)
        if cur not in threads:
            raise HarnessError(f"report: cpu {cpu}'s current thread {cur} has no thread line")
        frames = []
        for f in lines[i + 1 :]:
            fm = FRAME_LINE.match(f)
            if fm is None:
                break
            frames.append(fm.group(2))
        named = [f for f in frames if f != "?"]
        if not named:
            raise HarnessError(f"report: cpu {cpu} has no symbolized frame")
        if cpu > 0 and not any(WAIT_FRAME in f for f in named):
            raise HarnessError(f"report: cpu {cpu}'s frames lack {WAIT_FRAME}: {frames}")
    want = set(range(smp))
    if seen != want:
        raise HarnessError(f"report: cpu blocks {sorted(seen)}, want {sorted(want)}")
    log = next(((i, m) for i, m in enumerate(map(LOG_LINE.match, lines)) if m), None)
    if log is None:
        raise HarnessError("report: no `log:` line")
    at, m = log
    k, n = int(m.group(1)), int(m.group(2))
    if k != min(LOG_TAIL, n):
        raise HarnessError(f"report: log shows {k} of {n} records, want {min(LOG_TAIL, n)}")
    recs = lines[at + 1 : at + 1 + k]
    if len(recs) != k or not all(r.startswith("  [") for r in recs):
        raise HarnessError(f"report: {len(recs)} log records under the `log:` line, want {k}")
    if k == 0 or HANG_ARMED not in recs[-1]:
        raise HarnessError(f"report: last log record {recs[-1:]!r}, want {HANG_ARMED!r}")


def trace_order(report: str) -> str:
    """The report's `trace: order` value."""
    for ln in _lines(report):
        m = TRACE_LINE.match(ln)
        if m:
            return m.group(1)
    raise HarnessError("report: no `trace: order` line")


def check_export(path: Path, smp: int, order: str) -> None:
    """The trace export: JSON with a non-empty `traceEvents`, every event
    with `name`, `ph`, `ts`, `pid` and `tid`, each of the `smp` CPUs among
    the tracepoints (`tid` in one global timeline, `pid` per CPU), and an
    ordering statement that matches the report's."""
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        raise HarnessError(f"export: {path}: {e}") from e
    events = doc.get("traceEvents") if isinstance(doc, dict) else None
    if not isinstance(events, list) or not events:
        raise HarnessError("export: no traceEvents")
    for event in events:
        missing = [
            k
            for k in ("name", "ph", "ts", "pid", "tid")
            if not isinstance(event, dict) or k not in event
        ]
        if missing:
            raise HarnessError(f"export: event {event!r} lacks {missing}")
    other = doc.get("otherData", {})
    got = other.get("order") if isinstance(other, dict) else None
    if got != order:
        raise HarnessError(f"export: order {got!r}, the report says {order!r}")
    key = "tid" if order == "global" else "pid"
    cpus = {e[key] for e in events if e.get("ph") == "i"}
    want = set(range(smp))
    if not want <= cpus:
        raise HarnessError(f"export: tracepoints from cpus {sorted(cpus)}, want {sorted(want)}")


def _elf_loads(data: bytes) -> list[tuple[int, int, int, int]]:
    """Each `PT_LOAD` of an ELF64 little-endian file: (vaddr, memsz, offset, filesz)."""
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise HarnessError("virt: not an ELF64 little-endian file")
    (phoff,) = struct.unpack_from("<Q", data, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    out = []
    for i in range(phnum):
        p_type, _f, off, vaddr, _pa, filesz, memsz = struct.unpack_from(
            "<IIQQQQQ", data, phoff + i * phentsize
        )
        if p_type == 1:
            out.append((vaddr, memsz, off, filesz))
    return out


def _elf_section(data: bytes, name: str) -> tuple[int, int, int]:
    """(addr, offset, size) of section `name`."""
    (shoff,) = struct.unpack_from("<Q", data, 0x28)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)

    def sh(i: int) -> tuple[int, int, int, int]:
        n, _t, _fl, addr, off, size = struct.unpack_from("<IIQQQQ", data, shoff + i * shentsize)
        return n, addr, off, size

    _, _, stroff, _ = sh(shstrndx)
    for i in range(shnum):
        n, addr, off, size = sh(i)
        end = data.index(b"\0", stroff + n)
        if data[stroff + n : end].decode("ascii", errors="replace") == name:
            return addr, off, size
    raise HarnessError(f"virt: no {name} section")


def check_virt(virt: Path, elf: Path) -> None:
    """The virtual core covers every `PT_LOAD` of the kernel ELF, and holds
    the ELF's `.text` bytes at `.text`'s address."""
    core = virt.read_bytes()
    kern = elf.read_bytes()
    loads = _elf_loads(core)
    for vaddr, memsz, _off, _fs in _elf_loads(kern):
        pos, end = vaddr, vaddr + memsz
        while pos < end:
            seg = next((s for s in loads if s[0] <= pos < s[0] + s[1]), None)
            if seg is None:
                raise HarnessError(
                    f"virt: kernel VA {pos:#x} (a PT_LOAD of the ELF) is not in the virtual core"
                )
            pos = seg[0] + seg[1]
    addr, off, size = _elf_section(kern, ".text")
    want = kern[off : off + size]
    got = bytearray()
    pos = addr
    while len(got) < size:
        seg = next((s for s in loads if s[0] <= pos < s[0] + s[1]), None)
        if seg is None:
            raise HarnessError(f"virt: .text at {pos:#x} is not in the virtual core")
        take = min(size - len(got), seg[0] + seg[1] - pos)
        start = seg[2] + (pos - seg[0])
        got += core[start : start + take]
        pos += take
    if bytes(got) != want:
        raise HarnessError("virt: the virtual core's .text differs from the ELF's")


def check_gp_sig(report: str, lines: Sequence[str]) -> None:
    """The `#GP` core's signature: the serial exception line as `PanicLine`
    keeps it, numbers as `N`, then `gp_test_fault`, `gp_test_trip`,
    `boot_rest`."""
    gp = next(
        (t for t in map(frame.kernel_text, lines) if t and t.startswith("vibeOS: #GP rip=")), None
    )
    if gp is None:
        raise HarnessError("gp: no `vibeOS: #GP` line on serial")
    msg = normalize(panic_line(gp.removeprefix("vibeOS: ")))
    first = _lines(report)[:1]
    head = f"sig: {msg} @ "
    if not first or not first[0].startswith(head):
        raise HarnessError(f"gp: report line 1 {first!r}, want it to start {head!r}")
    frames = first[0][len(head) :].split(" < ")
    want = ("gp_test_fault", "gp_test_trip", "boot_rest")
    if len(frames) != 3 or not all(f.endswith(w) for f, w in zip(frames, want, strict=True)):
        raise HarnessError(f"gp: signature frames {frames}, want ones ending {list(want)}")


def core_bytes(report: str) -> int:
    for ln in _lines(report):
        m = CORE_LINE.match(ln)
        if m:
            return int(m.group(1))
    raise HarnessError("report: no `core:` line")


# ------------------------------------------------------------------- main


def tool(core: Path, elf: Path, *extra: str) -> tuple[int, str, str]:
    """`vmcore report` on `core` (streamed through `zstd -dc`)."""
    return run_vmcore(core, elf, extra)


def main() -> int:
    env = env_config(default_iso=default_iso("hang"), default_timeout=BOOT_ALLOWANCE_S)
    res = results.Results(env.tier)
    hang_iso = env.iso
    gp_iso = default_iso("gp")
    hang_elf = qmp.kernel_elf_for(hang_iso)
    gp_elf = qmp.kernel_elf_for(gp_iso)
    failed = 0

    def run_case(name: str, body: Callable[[], None]) -> None:
        nonlocal failed
        try:
            body()
        except (HarnessError, OSError) as e:
            res.record("marker", f"forensics_{name}", "failed")
            print(f"[forensics] FAIL: {name}: {e}", file=sys.stderr)
            failed += 1
            return
        res.record("marker", f"forensics_{name}", "passed")
        print(f"[forensics] ok: {name}", file=sys.stderr)

    state: dict[str, object] = {}

    def boot(label: str, cfg: QemuConfig, markers: list[Marker]) -> Boot:
        t0 = time.monotonic()
        b: Boot | None = None
        try:
            b = boot_and_capture(cfg, markers, OUT / label, label, env.timeout)
            return b
        finally:
            argv = b.argv if b is not None else []
            code = b.result.exit_code if b is not None else None
            res.add_boot(argv, cfg, code)
            print(
                f"[forensics] {label}: boot and core in {time.monotonic() - t0:.1f} s",
                file=sys.stderr,
            )

    def hang() -> None:
        cfg = dataclasses.replace(env.qemu(), iso=hang_iso, smp=SMP, expect="none")
        b = boot("hang", cfg, hang_markers(SMP))
        assert b.core is not None
        state["core"] = b.core
        virt, trace = OUT / "hang" / "core.virt", OUT / "hang" / "trace.json"
        rc, out, err = tool(b.core, hang_elf, "--virt", str(virt), "--trace", str(trace))
        if rc != 0:
            raise HarnessError(f"vmcore exited {rc}: {err.strip()}")
        print(out, file=sys.stderr)
        state["sig"] = out.splitlines()[0]
        check_report(out, SMP)
        check_export(trace, SMP, trace_order(out))
        check_virt(virt, hang_elf)

    def build_id() -> None:
        core = state.get("core")
        if not isinstance(core, Path):
            raise HarnessError("no hang core to check")
        rc, _out, err = tool(core, gp_elf)
        if rc != 3 or "vmcore: BUILD-ID mismatch" not in err:
            raise HarnessError(
                f"hang core with the gp ELF: exit {rc}, stderr {err.strip()!r}; "
                "want 3 and `vmcore: BUILD-ID mismatch`"
            )

    def gp() -> None:
        cfg = dataclasses.replace(env.qemu(), iso=gp_iso, expect="panic")
        b = boot("gp", cfg, gp_markers(cfg.smp))
        assert b.core is not None
        rc, out, err = tool(b.core, gp_elf)
        if rc != 0:
            raise HarnessError(f"vmcore exited {rc}: {err.strip()}")
        print(out.splitlines()[0] if out else "", file=sys.stderr)
        check_gp_sig(out, b.result.lines)

    def highmem() -> None:
        cfg = dataclasses.replace(env.qemu(), iso=hang_iso, smp=SMP, mem=HIGHMEM, expect="none")
        b = boot("highmem", cfg, hang_markers(SMP))
        assert b.core is not None
        rc, out, err = tool(b.core, hang_elf)
        if rc != 0:
            raise HarnessError(f"vmcore exited {rc}: {err.strip()}")
        first = out.splitlines()[:1]
        if first != [state.get("sig")]:
            raise HarnessError(
                f"highmem: signature {first!r}, the 128 MiB run's {state.get('sig')!r}"
            )
        if core_bytes(out) < HIGHMEM_BYTES:
            raise HarnessError(
                f"highmem: core holds {core_bytes(out)} bytes of RAM, want at least {HIGHMEM_BYTES}"
            )

    for name, body in (("hang", hang), ("build_id", build_id), ("gp", gp), ("highmem", highmem)):
        run_case(name, body)
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(results.run_main(main))
