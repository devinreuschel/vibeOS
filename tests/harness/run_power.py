#!/usr/bin/env python3
"""Power-off and restart through the `reboot` syscall (ROADMAP §10.5).

`make test-e2e-power` boots the `kernel_tests` ISO once per row of `ROWS`,
with `QemuConfig.ktest` naming that opt-in row, so the boot runs it alone.
The row runs `floorcheck poweroff` or `floorcheck restart`, whose `reboot`
call prints the row's line and ends the machine. A boot passes when it
shows, in order, the row's `run` line, then the row's registered line, then
QEMU exiting by itself with status 0: QEMU exits 0 on an ACPI power-off,
and on a reset under `-no-reboot`, so only the line tells the two apart
(QMP's `SHUTDOWN` and `RESET` events come with ROADMAP §20.2).

A boot fails on a panic signature or a user failure line, a `ktest: FAIL`,
the row's own result, a `ktest: end` (the call came back), the other row's
line, a nonzero exit (an odd one is `isa-debug-exit`'s, from the registry
ending the boot), or no progress by the ktest progress deadline
(`KtestDeadlines`). The harness retries nothing (ROADMAP §10.2).

Each boot runs under a `qmp.Session` declared `expect=none` (C-QMP), as
every harness boot does: QEMU starts halted with a QMP socket, a timeout
or a panic takes a guest core under `build/cores/` before QEMU stops
(ROADMAP §10.7), and the error carries the core tool's report.
"""

from __future__ import annotations

import dataclasses
import math
import os
import shutil
import sys
import time
from collections.abc import Callable
from dataclasses import dataclass, field

from tests.harness import frame, qmp, results
from tests.harness.harness import (
    BOOT_ALLOWANCE_S,
    IDLE_S,
    PANIC_DONE,
    HarnessError,
    KtestDeadlines,
    QemuProcess,
    RunResult,
    contains_panic,
    default_iso,
    env_config,
    ktest_devices,
    make_disk,
    parse_ktest_line,
    qemu_argv,
    qemu_system,
    serial_tail,
)
from tests.harness.linesource import LineSource
from tests.harness.run_ktest import DISK_BYTES, mmio_disk, unlink_disks

# Each opt-in row and the line its `reboot` call prints (markers.toml §10.5).
ROWS: tuple[tuple[str, str], ...] = (
    ("reboot_power_off", "vibeOS: reboot: power off"),
    ("reboot_restart", "vibeOS: reboot: restart"),
)
POWER_LINES = frozenset(line for _, line in ROWS)


@dataclass
class PowerBoot:
    """What one boot showed. `error` is None when it passed."""

    lines: list[str] = field(default_factory=list)
    exit_code: int | None = None
    error: str | None = None


def watch_power_boot(
    src: LineSource,
    row: str,
    line: str,
    progress: KtestDeadlines,
    clock: Callable[[], float] = time.monotonic,
    session: qmp.Session | None = None,
) -> PowerBoot:
    """Read one boot of `row` from `src` until QEMU exits, and judge it.

    With `session` (a started `qmp.Session`), a timeout or a panic takes a
    guest core before QEMU stops, as the event rule says (ROADMAP §10.7),
    and QMP is polled while serial is idle."""
    boot = PowerBoot()
    saw_run = False
    saw_line = False
    argv = list(getattr(src, "argv", []) or [])

    def ended(err: HarnessError) -> PowerBoot:
        boot.error = str(err)
        boot.exit_code = src.wait(5.0)
        return boot

    def fail(why: str) -> PowerBoot:
        msg = f"{row}: {why}{serial_tail(boot.lines)}"
        if session is not None:
            try:
                session.fail(src, RunResult(lines=boot.lines), argv, msg)
            except HarnessError as e:
                return ended(e)
        src.kill()
        boot.exit_code = src.wait(5.0)
        boot.error = msg
        return boot

    src.set_deadline(progress.start(clock()))
    while True:
        kind, raw = src.next_event()
        if kind == "idle":
            if session is not None:
                d = session.idle()
                if d:
                    try:
                        session.settle(src, RunResult(lines=boot.lines), d, argv)
                    except HarnessError as e:
                        return ended(e)
            continue
        if kind == "partial":
            boot.lines.append(raw)
            continue
        if kind == "timeout":
            why = f"{row}: no progress: {progress.hung_message()}{serial_tail(boot.lines)}"
            if session is not None:
                try:
                    session.timeout(src, RunResult(lines=boot.lines), argv, why)
                except HarnessError as e:
                    return ended(e)
            src.kill()
            boot.exit_code = src.wait(5.0)
            boot.error = why
            return boot
        if kind == "eof":
            break
        boot.lines.append(raw)
        if session is not None:
            halted = PANIC_DONE in (frame.kernel_text(raw) or "")
            d = session.line(raw, panic=contains_panic(raw), halted=halted)
            if d:
                try:
                    session.settle(
                        src, RunResult(lines=boot.lines), d, argv, why=f"{row}: panic line: {raw!r}"
                    )
                except HarnessError as e:
                    return ended(e)
        if contains_panic(raw):
            return fail(f"panic line: {raw!r}")
        user_fail = frame.user_failure(raw)
        if user_fail is not None:
            return fail(f"user failure {user_fail!r}: {raw!r}")
        k = parse_ktest_line(raw)
        if k is not None:
            if k.kind == "fail":
                return fail(f"ktest FAIL: {k.name}: {k.text}")
            if k.kind == "end":
                return fail("ktest: end before QEMU exited: the reboot call returned")
            if k.kind == "run" and k.name == row:
                saw_run = True
            if k.kind in ("ok", "skip") and k.name == row:
                return fail(f"the row ended with `{k.kind}`: the reboot call returned")
        text = frame.kernel_text(raw)
        if text is not None and text.rstrip() in POWER_LINES:
            if text.rstrip() != line:
                return fail(f"the other command's line: {text.rstrip()!r}")
            if not saw_run:
                return fail(f"{line!r} before the row's run line")
            saw_line = True
        src.set_deadline(progress.on_line(raw, clock()))
    code = src.wait(5.0)
    if code is None:
        src.kill()
        code = src.wait(5.0)
    boot.exit_code = code
    if not saw_run:
        boot.error = f"{row}: no `run {row}` line{serial_tail(boot.lines)}"
    elif not saw_line:
        boot.error = f"{row}: missing {line!r}{serial_tail(boot.lines)}"
    elif code != 0:
        how = "isa-debug-exit's" if code is not None and code % 2 == 1 else "not QEMU's own"
        boot.error = f"{row}: QEMU exited {code} ({how}), want 0{serial_tail(boot.lines)}"
    return boot


def main() -> int:
    env = env_config(default_iso=default_iso("ktest"), default_timeout=BOOT_ALLOWANCE_S)
    res = results.Results(env.tier)
    binary = qemu_system(env.arch)
    if not shutil.which(binary):
        print(f"[power] FAIL: {binary} not on PATH", file=sys.stderr)
        return 1
    if not os.path.exists(env.iso):
        print(f"[power] FAIL: ISO missing: {env.iso}", file=sys.stderr)
        return 1
    failed = 0
    for row, line in ROWS:
        disk = make_disk(DISK_BYTES, "vibeos-vblk-")
        mmio = mmio_disk(env.arch)
        try:
            base = env.qemu(
                extra=ktest_devices(disk, env.smp, arch=env.arch, mmio_disk=mmio),
                boot_order="d",
            )
            cfg = dataclasses.replace(base, ktest=row)
            session = qmp.Session(cfg, row)
            argv = qemu_argv(cfg, None, qmp_sock=session.sock)
            src = QemuProcess(argv, math.inf, idle_s=IDLE_S)
            try:
                session.start()
                deadlines = KtestDeadlines(env.timeout, env.timeout_scale)
                boot = watch_power_boot(src, row, line, deadlines, session=session)
            finally:
                session.close()
                if src.wait(0.0) is None:
                    src.kill()
                    src.wait(5.0)
        except HarnessError as e:
            res.record("ktest", row, "failed")
            print(f"[power] FAIL: {row}: {e}", file=sys.stderr)
            failed += 1
            continue
        finally:
            unlink_disks(disk, mmio)
        res.add_boot(argv, cfg, boot.exit_code)
        if boot.error is not None:
            res.record("ktest", row, "failed")
            print(f"[power] FAIL: {boot.error}", file=sys.stderr)
            failed += 1
            continue
        res.record("ktest", row, "passed")
        res.record("marker", line, "passed")
        print(f"[power] ok: {row}: {line!r}, QEMU exited 0", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(results.run_main(main))
