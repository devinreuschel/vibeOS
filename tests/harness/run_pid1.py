#!/usr/bin/env python3
"""Pid 1's end panics the kernel with its registered line (ROADMAP §10.5, F068).

`make test-e2e-init-fault` boots a variant ISO whose initrd holds, as
`/sbin/init`, a program that stores to address `0x1000` (the production
`--add` list with that one entry replaced; the faulting init is never in
the production initrd, AGENTS.md rule 9). Each entry of `CASES` names the
ISO variant it boots and the needles its run requires. A run goes through
`run_qemu_and_check` with `expect="panic"` and the boot contract's
kernel rows, which all come before init starts, and ends on QMP's
`GUEST_PANICKED` from the kernel's pvpanic write; then the framed
`vibeOS: init: pid 1 <how>` line must name the case's end, and
`vibeOS: panic: halted` must follow it. The run fails on `user: tests ok`
or `vibeOS: shell ready` (a working init ran), on the unframed
`user: pid 1 killed ...` diagnostic standing in for the framed line, on
any other end, and on a timeout. The harness retries nothing (ROADMAP
§10.2).
"""

from __future__ import annotations

import sys
from dataclasses import dataclass

from tests.harness import frame, results
from tests.harness.harness import (
    BOOT_ALLOWANCE_S,
    PANIC_DONE,
    EnvConfig,
    HarnessError,
    RunResult,
    boot_contract_markers,
    default_iso,
    env_config,
    qemu_argv,
    run_qemu_and_check,
    serial_tail,
)
from tests.harness.linesource import LineSource
from tests.harness.qmp import QmpLike

# The registered line's head (markers.toml §10.5, `pid1_exit`).
PID1_PREFIX = "vibeOS: init: pid 1 "

# Lines only a working init's boot prints.
NOT_AFTER_INIT = ("user: tests ok", "vibeOS: shell ready")


@dataclass(frozen=True)
class Case:
    """One pid-1 case: the ISO variant it boots and the end its line names."""

    variant: str
    how: str


CASES: dict[str, Case] = {
    "init_fault": Case(variant="init-fault", how="killed SIGSEGV addr=0x1000"),
}


def check_pid1_lines(name: str, lines: list[str]) -> None:
    """The case's framed pid-1 line, then `PANIC_DONE`; no working init."""
    case = CASES[name]
    want = PID1_PREFIX + case.how
    for raw in lines:
        user = frame.user_text(raw)
        if user is not None and any(n in user for n in NOT_AFTER_INIT):
            raise HarnessError(f"{name}: a working init's line: {raw!r}{serial_tail(lines)}")
    at: int | None = None
    for i, raw in enumerate(lines):
        text = frame.kernel_text(raw)
        if text is None or not text.startswith(PID1_PREFIX):
            continue
        if text.rstrip() != want:
            raise HarnessError(f"{name}: {text.rstrip()!r}, want {want!r}{serial_tail(lines)}")
        at = i
        break
    if at is None:
        raise HarnessError(f"{name}: no framed {want!r} line{serial_tail(lines)}")
    if not any(
        (t := frame.kernel_text(raw)) is not None and PANIC_DONE in t for raw in lines[at + 1:]
    ):
        raise HarnessError(f"{name}: no {PANIC_DONE!r} after {want!r}{serial_tail(lines)}")


def run_case(
    name: str,
    env: EnvConfig,
    line_source: LineSource | None = None,
    qmp: QmpLike | None = None,
) -> RunResult:
    """Boot `name`'s ISO and check its pid-1 line (`HarnessError` on a failure)."""
    cfg = env.qemu(expect="panic")
    markers = [
        m
        for m in boot_contract_markers(cpu=env.cpu, smp=env.smp)
        if (m.source or frame.source_of(m.substring)) == frame.KERNEL
    ]
    result = run_qemu_and_check(
        cfg,
        markers,
        timeout_s=env.timeout,
        line_source=line_source,
        qmp=qmp,
    )
    check_pid1_lines(name, result.lines)
    return result


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 1 or args[0] not in CASES:
        print(f"usage: run_pid1.py <case>, one of {sorted(CASES)}", file=sys.stderr)
        return 2
    name = args[0]
    env = env_config(default_iso=default_iso(CASES[name].variant), default_timeout=BOOT_ALLOWANCE_S)
    res = results.Results(env.tier)
    cfg = env.qemu(expect="panic")
    try:
        result = run_case(name, env)
    except HarnessError as e:
        res.record("marker", "pid1_exit", "failed")
        res.add_boot(qemu_argv(cfg, None), cfg, None)
        print(f"[pid1] FAIL: {e}", file=sys.stderr)
        return 1
    for m in result.matched:
        res.record("marker", m, "passed")
    res.record("marker", "pid1_exit", "passed")
    res.add_boot(qemu_argv(cfg, None), cfg, result.exit_code)
    print(
        f"[pid1] ok: {name}: {PID1_PREFIX}{CASES[name].how}, then {PANIC_DONE!r}; "
        f"the run ended on {result.end}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(results.run_main(main))
