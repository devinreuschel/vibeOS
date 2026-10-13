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

`init_no_sh` (`init_no_sh_case`) boots the `nosh` ISO, whose initrd has no
`/bin/sh` and has `/bin/false` as `/bin/tests` (the production list with
those two entries changed). It requires, in order, init's
`init: /bin/tests exited` line for exit 1, exactly three
`init: /bin/sh start failed: execve errno 2` lines, and the framed pid-1
line for `exited 1` followed by `vibeOS: panic: halted`, each built from
its `markers.toml` row (F128). Its run reads through the `/bin/tests` line,
which fails every other boot, so it does not use `run_qemu_and_check`'s
failure scan.
"""

from __future__ import annotations

import sys
from dataclasses import dataclass
from typing import NoReturn

from tests.harness import frame, registry, results
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
    run_qemu_until_exit,
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


# The no-/bin/sh case's name, ISO variant, and its rows in markers.toml.
NO_SH = "init_no_sh"
NO_SH_VARIANT = "nosh"
TESTS_EXITED_ROW = "init: /bin/tests exited <status>"
SH_START_FAILED_ROW = "init: /bin/sh start failed: <why>"
PID1_ROW = "vibeOS: init: pid 1 <text>"
# Failed `/bin/sh` starts init makes before it exits (L1365).
SH_STARTS = 3


def no_sh_needles() -> tuple[str, str, str]:
    """The whole lines `init_no_sh` requires, from their `markers.toml` rows:
    `/bin/false`'s status word (exit 1, 256), `ENOENT`'s errno, and init's
    exit 1."""
    texts = {row.text for row in registry.load_rows()}
    for t in (TESTS_EXITED_ROW, SH_START_FAILED_ROW, PID1_ROW):
        if t not in texts:
            raise HarnessError(f"{NO_SH}: no markers.toml row {t!r}")
    return (
        registry.bind(TESTS_EXITED_ROW, {"status": "256"}),
        registry.bind(SH_START_FAILED_ROW, {"why": "execve errno 2"}),
        registry.bind(PID1_ROW, {"text": "exited 1"}),
    )


def check_no_sh_lines(lines: list[str]) -> None:
    """The `/bin/tests` line, then exactly `SH_STARTS` failed-start lines,
    then the framed pid-1 line and `PANIC_DONE`; no working init's line."""
    tests, sh, pid1 = no_sh_needles()

    def fail(why: str) -> NoReturn:
        raise HarnessError(f"{NO_SH}: {why}{serial_tail(lines)}")

    user = [(i, t.rstrip()) for i, raw in enumerate(lines) if (t := frame.user_text(raw))]
    for _, text in user:
        if any(n in text for n in NOT_AFTER_INIT):
            fail(f"a working init's line: {text!r}")
    at_tests = next((i for i, t in user if t == tests), None)
    if at_tests is None:
        fail(f"no {tests!r} line")
    starts = [i for i, t in user if t == sh]
    if len(starts) != SH_STARTS:
        fail(f"{len(starts)} {sh!r} lines, want {SH_STARTS}")
    if starts[0] < at_tests:
        fail(f"{sh!r} before {tests!r}")
    at_pid1 = next(
        (i for i, raw in enumerate(lines) if (frame.kernel_text(raw) or "").rstrip() == pid1),
        None,
    )
    if at_pid1 is None:
        fail(f"no framed {pid1!r} line")
    if at_pid1 < starts[-1]:
        fail(f"{pid1!r} before the last {sh!r}")
    if not any(PANIC_DONE in (frame.kernel_text(raw) or "") for raw in lines[at_pid1 + 1:]):
        fail(f"no {PANIC_DONE!r} after {pid1!r}")


def init_no_sh_case(env: EnvConfig) -> RunResult:
    """Boot the `nosh` ISO and check its lines (`HarnessError` on a failure).

    The run declares the panic's end (`expect="panic"`) and reads through
    every failing line (`expect_fail`), so it ends on QMP's
    `GUEST_PANICKED` or its timeout, and `check_no_sh_lines` judges it."""
    cfg = env.qemu(expect="panic")
    result = run_qemu_until_exit(cfg, timeout_s=env.timeout, expect_fail=True)
    check_no_sh_lines(result.lines)
    return result


def no_sh_main() -> int:
    """`run_pid1.py init_no_sh`: run the case and record `init_no_sh`."""
    env = env_config(default_iso=default_iso(NO_SH_VARIANT), default_timeout=BOOT_ALLOWANCE_S)
    res = results.Results(env.tier, env.arch)
    cfg = env.qemu(expect="panic")
    try:
        result = init_no_sh_case(env)
    except HarnessError as e:
        res.record("marker", NO_SH, "failed")
        res.add_boot(qemu_argv(cfg, None), cfg, None)
        print(f"[pid1] FAIL: {e}", file=sys.stderr)
        return 1
    res.record("marker", NO_SH, "passed")
    res.add_boot(qemu_argv(cfg, None), cfg, result.exit_code)
    print(
        f"[pid1] ok: {NO_SH}: the /bin/tests line, {SH_STARTS} failed /bin/sh starts, then "
        f"{PID1_PREFIX}exited 1 and {PANIC_DONE!r}",
        file=sys.stderr,
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if args == [NO_SH]:
        return no_sh_main()
    if len(args) != 1 or args[0] not in CASES:
        print(f"usage: run_pid1.py <case>, one of {sorted([*CASES, NO_SH])}", file=sys.stderr)
        return 2
    name = args[0]
    env = env_config(default_iso=default_iso(CASES[name].variant), default_timeout=BOOT_ALLOWANCE_S)
    res = results.Results(env.tier, env.arch)
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
