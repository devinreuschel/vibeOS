#!/usr/bin/env python3
"""In-guest kernel test runner. DESIGN §8.2. Driven by `make test-kernel`."""

from __future__ import annotations

import os
import sys
from collections.abc import Callable

from tests.harness.harness import (
    HarnessError,
    QemuConfig,
    RunResult,
    check_ktest_output,
    effective_accel_name,
    env_config,
    env_flag,
    ktest_devices,
    make_disk,
    retryable_ktest_failure,
    run_qemu_until_exit,
    silent_user_syscalls_hang,
)


DISK_BYTES = 4 * 1024 * 1024


def _require_line(lines: list[str], pred: Callable[[str], bool], msg: str) -> None:
    if not any(pred(ln) for ln in lines):
        raise HarnessError(msg)


def _block_name(name: str) -> Callable[[str], bool]:
    def pred(ln: str) -> bool:
        bits = ln.split()
        return (
            len(bits) == 5
            and bits[0] == "vibeOS:"
            and bits[1] == "block:"
            and bits[2] == name
            and bits[4] == "sectors"
        )

    return pred


def _ktest_boot(cfg: QemuConfig, timeout: float, *, persist_reboot: bool) -> RunResult:
    """One ktest QEMU. Retry once on a known host-timing flake.

    The #75 `user: dup ok` wait4 stall can survive that first retry under
    periodic LAPIC; give that exact timeout class one extra attempt.
    """
    tag = "persist reboot" if persist_reboot else "ktest"
    last: HarnessError | None = None
    for attempt in range(3):
        raw = None
        try:
            raw = run_qemu_until_exit(cfg, timeout_s=timeout)
            check_ktest_output(raw.lines, raw.exit_code)
            _require_line(raw.lines, _block_name("vda"), "missing virtio-blk marker")
            _require_line(raw.lines, _block_name("vdap1"), "missing vdap1 marker")
            if persist_reboot:
                _require_line(
                    raw.lines,
                    lambda ln: ln == "vibeOS: persist: intact",
                    "persist pattern did not survive reboot",
                )
            else:
                _require_line(raw.lines, _block_name("vdap2"), "missing vdap2 marker")
                _require_line(
                    raw.lines,
                    lambda ln: ln == "vibeOS: persist: wrote",
                    "missing persist wrote",
                )
            return raw
        except HarnessError as e:
            last = e
            timed_out = "timed out" in str(e)
            failure_lines = (
                [
                    line
                    for line in raw.lines
                    if line.startswith("vibeOS: ktest: FAIL")
                ]
                if raw is not None
                else []
            )
            retryable = retryable_ktest_failure(
                cfg.smp,
                str(e),
                persist_reboot=persist_reboot,
                accel=effective_accel_name(cfg),
                failure_lines=failure_lines,
            )
            extra_dup_ok = timed_out and silent_user_syscalls_hang(str(e))
            if (attempt == 0 and (timed_out or retryable)) or (
                attempt == 1 and extra_dup_ok
            ):
                reason = "timeout" if timed_out else "known ktest timing flake"
                print(f"[{tag}] retry after {reason}: {e}", file=sys.stderr)
                continue
            raise
    assert last is not None
    raise last


def main() -> int:
    env = env_config(default_iso="vibeos-ktest.iso", default_timeout=90)
    skip_persist = env_flag("VIBEOS_SKIP_PERSIST")
    disk = make_disk(DISK_BYTES, "vibeos-vblk-")
    try:
        cfg = env.qemu(extra=ktest_devices(disk, env.smp), boot_order="d")
        try:
            raw = _ktest_boot(cfg, env.timeout, persist_reboot=False)
        except HarnessError as e:
            print(f"[ktest] FAIL: {e}", file=sys.stderr)
            return 1

        oks = [ln for ln in raw.lines if ln.startswith("vibeOS: ktest: ok ")]
        skips = [ln for ln in raw.lines if ln.startswith("vibeOS: ktest: skip ")]
        print(
            f"[ktest] ok: {len(oks)} passed, {len(skips)} skipped, exit {raw.exit_code}",
            file=sys.stderr,
        )
        for ln in oks + skips:
            print(f"[ktest]   . {ln}", file=sys.stderr)

        if skip_persist:
            return 0

        try:
            _ktest_boot(cfg, env.timeout, persist_reboot=True)
        except HarnessError as e:
            print(f"[ktest] FAIL persist reboot: {e}", file=sys.stderr)
            return 1

        print("[ktest] persist reboot: intact", file=sys.stderr)
        return 0
    finally:
        try:
            os.unlink(disk)
        except OSError:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
