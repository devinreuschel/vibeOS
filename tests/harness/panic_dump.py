"""Checks over a panic-path variant's serial log (ROADMAP §10.7).

Each takes the run's raw lines and reads only kernel lines (framed, DESIGN
§2.6), so a user program's copy of a dump line never counts; each raises
`HarnessError` naming what is wrong. `run_e2e` calls them after
`run_qemu_and_check` has found the dump, its one banner and the panic exit.
"""

from __future__ import annotations

import re
from collections.abc import Iterable

from tests.harness import frame
from tests.harness.harness import DUMP_BANNER_RE, PANIC_DONE, HarnessError

MSG_PREFIX = "vibeOS: panic: msg: "
REENTERED = "vibeOS: panic: reentered"
NEST_MSG = MSG_PREFIX + "irq nest underflow"


def _count_banners(lines: list[str]) -> int:
    return sum(1 for t in lines if DUMP_BANNER_RE.match(t) is not None)


def _once(lines: list[str], text: str, what: str) -> None:
    n = sum(1 for t in lines if t == text)
    if n != 1:
        raise HarnessError(f"{what}: {n} {text!r} lines, expected one")


def _common(lines: list[str], what: str) -> None:
    """No `reentered`, one banner, one `msg:` line, one `halted`."""
    if any(t.startswith(REENTERED) for t in lines):
        raise HarnessError(f"{what}: the dump re-entered ({REENTERED!r})")
    n = _count_banners(lines)
    if n != 1:
        raise HarnessError(f"{what}: {n} dump banners, expected one")
    msgs = [t for t in lines if t.startswith(MSG_PREFIX)]
    if len(msgs) != 1:
        raise HarnessError(f"{what}: {len(msgs)} {MSG_PREFIX!r} lines, expected one")
    _once(lines, PANIC_DONE, what)


def check_nest(raw: Iterable[str]) -> None:
    """F071: the `irq_nest` underflow's own message, dumped once."""
    lines = frame.kernel_lines(raw)
    _common(lines, "panic-nest")
    if NEST_MSG not in lines:
        raise HarnessError(f"panic-nest: no {NEST_MSG!r} line")


THREAD_RE = re.compile(r"^vibeOS: panic: thread cpu=(\d+) ")
CPU_RE = re.compile(r"^vibeOS: panic: cpu (\d+) (stopped \((\w+)\)|not stopped)$")
NUMBERED = "vibeOS: panic_stop: line "
OWNER_NMI = "vibeOS: panic_stop: owner nmi returned"
FRAME_PREFIX = "  0x"


def check_stop(raw: Iterable[str]) -> None:
    """F135 at `-smp 5`: the owner (CPU 0 or 1, from its `thread` line)
    reports the other of the pair `stopped (panic)`, CPUs 2 and 3
    `stopped (poll)` and CPU 4 `stopped (nmi)`, none `not stopped`; one
    message, banner and `halted`; a backtrace frame; the owner-NMI line; and
    no numbered line after the first `vibeOS: panic:` line (a `logrec:`
    replay of one does not count: it starts with `vibeOS: logrec:`)."""
    lines = frame.kernel_lines(raw)
    _common(lines, "panic-stop")
    first = next((i for i, t in enumerate(lines) if t.startswith("vibeOS: panic:")), None)
    if first is None:
        raise HarnessError("panic-stop: no 'vibeOS: panic:' line")
    late = [t for t in lines[first:] if t.startswith(NUMBERED)]
    if late:
        raise HarnessError(f"panic-stop: {late[0]!r} after the first panic line")
    owners = [m.group(1) for m in (THREAD_RE.match(t) for t in lines) if m is not None]
    if len(owners) != 1 or owners[0] not in ("0", "1"):
        raise HarnessError(f"panic-stop: owner cpus {owners!r}, expected one of 0 and 1")
    owner = int(owners[0])
    want = {1 - owner: "panic", 2: "poll", 3: "poll", 4: "nmi"}
    got: dict[int, str] = {}
    for t in lines:
        m = CPU_RE.match(t)
        if m is None:
            continue
        cpu = int(m.group(1))
        if cpu in got:
            raise HarnessError(f"panic-stop: cpu {cpu} reported twice")
        got[cpu] = m.group(3) if m.group(3) is not None else "not stopped"
    for cpu, how in sorted(want.items()):
        if got.get(cpu) != how:
            seen = got.get(cpu, "no line")
            raise HarnessError(f"panic-stop: cpu {cpu} {seen!r}, expected 'stopped ({how})'")
    extra = sorted(set(got) - set(want))
    if extra:
        raise HarnessError(f"panic-stop: unexpected cpu lines for {extra!r}")
    if not any(t.startswith(FRAME_PREFIX) for t in lines[first:]):
        raise HarnessError("panic-stop: no backtrace frame")
    if OWNER_NMI not in lines:
        raise HarnessError(f"panic-stop: no {OWNER_NMI!r} line: the owner's own NMI did not return")


def frame_symbols(raw: Iterable[str]) -> list[str]:
    """The symbol of each backtrace frame line (`  0x<addr> <symbol>[+0x<off>]`),
    in order; a frame the table did not name gives ""."""
    out: list[str] = []
    for t in frame.kernel_lines(raw):
        if not t.startswith(FRAME_PREFIX):
            continue
        parts = t.split()
        out.append(parts[1].split("+0x", 1)[0] if len(parts) > 1 else "")
    return out


def _names(sym: str, name: str) -> bool:
    return sym == name or sym.endswith("::" + name)


def check_frames_in_order(raw: Iterable[str], names: tuple[str, ...]) -> None:
    """F070: the backtrace has a frame naming each of `names` (a function's
    last path segment), each after the one naming the name before it."""
    syms = frame_symbols(raw)
    at = 0
    for name in names:
        i = next((j for j in range(at, len(syms)) if _names(syms[j], name)), None)
        if i is None:
            raise HarnessError(f"backtrace: no frame naming {name!r} in order {names!r}: {syms!r}")
        at = i + 1
