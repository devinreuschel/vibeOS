"""Checks over a panic-path variant's serial log (ROADMAP §10.7).

Each takes the run's raw lines and reads only kernel lines (framed, DESIGN
§2.6), so a user program's copy of a dump line never counts; each raises
`HarnessError` naming what is wrong. `run_e2e` calls them after
`run_qemu_and_check` has found the dump, its one banner and the panic exit.
"""

from __future__ import annotations

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
