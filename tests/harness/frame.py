"""Kernel lines and user lines on the console UART (DESIGN §2.6, C-FRAME).

Every line the kernel writes to its console UART starts with `FRAME`
(0x1E), and a `\\r`, `\\n` or 0x1E inside it prints as `?`. The bytes a
process writes to the console carry no frame, and a 0x1E among them prints
as `?`, so ring 3 cannot forge a kernel line. Every driver classifies each
raw serial line here: contract markers, ktest verdicts and the panic
signatures match only kernel text (a framed line, frame stripped), and the
lines user programs print only user text (an unframed line).

Limine's panic line comes before the kernel and cannot be framed: before
the first framed line of a boot, an unframed line that matches
`LIMINE_PANIC` fails the run (`Stream.limine_panic`). After it, the same
text is just a user line.

Standard library only, and nothing from `harness.py`, which imports this.
"""

from __future__ import annotations

import re
from collections.abc import Iterable

FRAME = "\x1e"

# The sources a line can come from (`text_for`).
KERNEL = "kernel"
USER = "user"
LIMINE = "limine"

# Needles that name lines user programs print, which match only unframed
# text (`source_of`). The pre-registry source table: ROADMAP §10.2's
# markers.toml `source` column replaces it.
USER_PREFIXES: tuple[str, ...] = (
    "utest",
    "vibeOS: utest:",
    "user: tests begin",
    "user: tests ok",
    "user: tests fail",
    "user: dup ok",
    "init: ",
    "vibeOS: shell ready",
)

# Limine's panic line: `PANIC`, optional ANSI colour codes, then `: `, as
# `limine-bios.sys` prints `\x1b[31mPANIC\x1b[37;1m\x1b[0m: ` or `PANIC: `.
LIMINE_PANIC = re.compile(r"PANIC(?:\x1b\[[0-9;]*m)*: ")

# The user failure tuple, matched on unframed lines, and the Limine tuple,
# matched before the first framed line. Every driver that checks the
# kernel's `PANIC_SIGNATURES` (on framed lines) checks both too.
USER_FAILURES: tuple[str, ...] = ("user: tests fail",)
LIMINE_SIGNATURES: tuple[re.Pattern[str], ...] = (LIMINE_PANIC,)


def split_frame(raw: str) -> tuple[bool, str]:
    """(framed, text): whether the kernel printed `raw`, and `raw` without its frame."""
    if raw.startswith(FRAME):
        return True, raw[len(FRAME) :]
    return False, raw


def kernel_text(raw: str) -> str | None:
    """The text of a kernel line, or None for a line the kernel did not print."""
    framed, text = split_frame(raw)
    return text if framed else None


def user_text(raw: str) -> str | None:
    """The text of an unframed line (a user program's, the loader's), or None."""
    framed, text = split_frame(raw)
    return None if framed else text


def text_for(source: str, raw: str) -> str | None:
    """`raw`'s text when it comes from `source`, else None."""
    if source == KERNEL:
        return kernel_text(raw)
    if source in (USER, LIMINE):
        return user_text(raw)
    raise ValueError(f"unknown line source {source!r}")


def kernel_lines(lines: Iterable[str]) -> list[str]:
    """The text of each kernel line, in order."""
    return [t for t in (kernel_text(ln) for ln in lines) if t is not None]


def user_lines(lines: Iterable[str]) -> list[str]:
    """The text of each unframed line, in order."""
    return [t for t in (user_text(ln) for ln in lines) if t is not None]


def source_of(needle: str) -> str:
    """`USER` for a needle a user program prints (`USER_PREFIXES`), else `KERNEL`."""
    return USER if needle.startswith(USER_PREFIXES) else KERNEL


def user_failure(raw: str) -> str | None:
    """The `USER_FAILURES` entry in `raw`'s user text, if any."""
    text = user_text(raw)
    if text is None:
        return None
    return next((f for f in USER_FAILURES if f in text), None)


class Stream:
    """One boot's serial, line by line: remembers whether a framed line was seen."""

    def __init__(self) -> None:
        self.kernel_seen = False

    def feed(self, raw: str) -> tuple[bool, str]:
        """`split_frame(raw)`, noting the first framed line."""
        framed, text = split_frame(raw)
        if framed:
            self.kernel_seen = True
        return framed, text

    def limine_panic(self, raw: str) -> bool:
        """True for an unframed Limine panic line before the boot's first framed line."""
        if self.kernel_seen:
            return False
        framed, text = split_frame(raw)
        return not framed and any(p.search(text) for p in LIMINE_SIGNATURES)
