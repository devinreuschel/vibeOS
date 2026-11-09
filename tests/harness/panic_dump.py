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


EARLY_ARMED = "vibeOS: boot: early-fault armed"
EARLY_RE = re.compile(
    r"^vibeOS: panic: early vec=0x([0-9a-f]+) err=0x[0-9a-f]+ pc=0x([0-9a-f]+) "
    r"addr=0x[0-9a-f]+ sp=0x[0-9a-f]+$"
)
# The early-fault build's undefined instruction: `ud2` is `#UD`, vector 6;
# `udf #0` is exception class 0x00, unknown reason.
EARLY_VEC = {"x86_64": 6, "aarch64": 0}


def check_early(raw: Iterable[str], arch: str) -> None:
    """F136: the early handler's one line, after the armed line, naming the
    undefined instruction's vector or class and a PC, then one `halted`,
    and no other dump banner."""
    lines = frame.kernel_lines(raw)
    if EARLY_ARMED not in lines:
        raise HarnessError(f"early-fault: no {EARLY_ARMED!r} line")
    after = lines[lines.index(EARLY_ARMED) + 1 :]
    early = [m for m in (EARLY_RE.match(t) for t in after) if m is not None]
    if len(early) != 1:
        raise HarnessError(f"early-fault: {len(early)} early-handler lines, expected one")
    vec, pc = (int(g, 16) for g in early[0].groups())
    if vec != EARLY_VEC[arch]:
        raise HarnessError(f"early-fault: vec={vec:#x}, expected {EARLY_VEC[arch]:#x} on {arch}")
    if pc == 0:
        raise HarnessError("early-fault: pc=0x0")
    # The early line is the one banner: the early handler dumps nothing more.
    n = _count_banners(lines)
    if n != 1:
        raise HarnessError(f"early-fault: {n} dump banners, expected the early line alone")
    _once(lines, PANIC_DONE, "early-fault")


OVERFLOW_ARMED_RE = re.compile(
    r"^vibeOS: boot: kstack-overflow armed guard=0x([0-9a-f]+) base=0x([0-9a-f]+) "
    r"thread=0x([0-9a-f]+)$"
)
OVERFLOW_RE = re.compile(
    r"^vibeOS: panic: stack overflow far=0x([0-9a-f]+) esr=0x([0-9a-f]+) "
    r"elr=0x([0-9a-f]+) sp=0x([0-9a-f]+) thread=0x([0-9a-f]+)$"
)
# ESR_EL1.EC of a data abort taken at EL1 without a change of level.
EC_DABT_CUR = 0x25


def check_overflow(raw: Iterable[str], arch: str) -> None:
    """ROADMAP §11.3: after the armed line, which names the thread's guard
    `[guard, base)` and its TCB, one `stack overflow` line whose fault
    address and SP lie in that guard (the SP at most at `base`: on x86_64
    the `#DF` frame's RSP is the one before the push that faulted), whose
    thread is the armed one, with a nonzero ELR, and an ESR that names the
    fault (a current-EL data abort on aarch64, `#DF`'s zero error code on
    x86_64, by `arch`); then one `halted`."""
    lines = frame.kernel_lines(raw)
    armed = [(i, m) for i, m in enumerate(OVERFLOW_ARMED_RE.match(t) for t in lines) if m]
    if len(armed) != 1:
        raise HarnessError(f"kstack-overflow: {len(armed)} armed lines, expected one")
    at, m = armed[0]
    guard, base, thread = (int(g, 16) for g in m.groups())
    if not 0 < guard < base:
        raise HarnessError(f"kstack-overflow: armed guard {guard:#x}..{base:#x} is empty")
    hits = [(i, o) for i, o in enumerate(OVERFLOW_RE.match(t) for t in lines) if o]
    if len(hits) != 1 or hits[0][0] < at:
        raise HarnessError(
            f"kstack-overflow: {len(hits)} overflow lines, expected one after the armed line"
        )
    hit, o = hits[0]
    far, esr, elr, sp, tid = (int(g, 16) for g in o.groups())
    if not guard <= far < base:
        raise HarnessError(f"kstack-overflow: far={far:#x} outside the guard {guard:#x}..{base:#x}")
    if not guard <= sp <= base:
        raise HarnessError(f"kstack-overflow: sp={sp:#x} outside the guard {guard:#x}..{base:#x}")
    if tid != thread:
        raise HarnessError(f"kstack-overflow: thread={tid:#x}, armed {thread:#x}")
    if elr == 0:
        raise HarnessError("kstack-overflow: elr=0x0")
    aarch64 = arch == "aarch64"
    if aarch64 and (esr >> 26) & 0x3F != EC_DABT_CUR:
        raise HarnessError(f"kstack-overflow: esr={esr:#x} is not a current-EL data abort")
    if not aarch64 and esr != 0:
        raise HarnessError(f"kstack-overflow: esr={esr:#x}, #DF's error code is 0")
    _once(lines, PANIC_DONE, "kstack-overflow")
    if lines.index(PANIC_DONE) < hit:
        raise HarnessError("kstack-overflow: halted before the overflow line")


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
    `stopped (poll)` and CPU 4 `stopped (nmi)` on x86_64 (`not stopped` on
    aarch64, which has no NMI), none of the others `not stopped`; one
    message, banner and `halted`; a backtrace frame; the owner-NMI line on
    x86_64; and no numbered line after the first `vibeOS: panic:` line (a
    `logrec:` replay of one does not count: it starts with `vibeOS: logrec:`)."""
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
    aarch64 = any(t.startswith("vibeOS: el:") or t.startswith("vibeOS: gic:") for t in lines)
    want = {1 - owner: "panic", 2: "poll", 3: "poll", 4: "nmi"}
    if aarch64:
        want[4] = "not stopped"
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
            expect = how if how == "not stopped" else f"stopped ({how})"
            raise HarnessError(f"panic-stop: cpu {cpu} {seen!r}, expected {expect!r}")
    extra = sorted(set(got) - set(want))
    if extra:
        raise HarnessError(f"panic-stop: unexpected cpu lines for {extra!r}")
    if not any(t.startswith(FRAME_PREFIX) for t in lines[first:]):
        raise HarnessError("panic-stop: no backtrace frame")
    if not aarch64 and OWNER_NMI not in lines:
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
