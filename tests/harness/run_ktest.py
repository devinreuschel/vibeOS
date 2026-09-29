#!/usr/bin/env python3
"""In-guest kernel test runner. DESIGN §8.2. Driven by `make test-kernel`."""

from __future__ import annotations

import os
import re
import sys
from collections.abc import Callable

from tests.harness import frame, results
from tests.harness.harness import (
    HarnessError,
    QemuConfig,
    RunResult,
    check_ktest_output,
    env_config,
    env_flag,
    ktest_devices,
    make_disk,
    qemu_argv,
    run_qemu_until_exit,
)

DISK_BYTES = 4 * 1024 * 1024

# `serial_lines_whole` (ROADMAP §10.2, F138): CPU 0 prints SERIAL_WHOLE_N
# numbered lines while every AP prints noise lines; each ends in SERIAL_PAD.
SERIAL_WHOLE_OK = "vibeOS: ktest: ok serial_lines_whole"
SERIAL_WHOLE_N = 1000
SERIAL_PAD = "0123456789abcdefghijklmnopqrstuvwxyz"
_SERIAL_WHOLE_RE = re.compile(
    rf"vibeOS: ktest: serial whole (\d+) of {SERIAL_WHOLE_N} {SERIAL_PAD}"
)
_SERIAL_NOISE_RE = re.compile(rf"vibeOS: ktest: serial noise (?:klog )?cpu\d+ \d+ {SERIAL_PAD}")
# A log-ring replay of one of those lines: `dmesg`, or a panic dump's logrec.
_RING_REPLAY_RE = re.compile(r"vibeOS: (?:dmesg|logrec): \S+ cpu\d+ \w+ (.*)")


def _require_line(lines: list[str], pred: Callable[[str], bool], msg: str) -> None:
    if not any(pred(ln) for ln in lines):
        raise HarnessError(msg)


def _check_serial_whole(lines: list[str]) -> None:
    """Each numbered `serial whole` line appears once, whole (ROADMAP §10.2, F138).

    `lines` are kernel text (`frame.kernel_lines`).

    A line that holds `serial whole` or `serial noise` but is not exactly one
    such line is a fragment of a line another CPU split. A log-ring replay
    of a whole line (`dmesg`, `logrec`) is checked for fragments but not
    counted.
    """
    seen = [0] * SERIAL_WHOLE_N
    for ln in lines:
        if "serial whole" not in ln and "serial noise" not in ln:
            continue
        replay = _RING_REPLAY_RE.fullmatch(ln)
        text = replay.group(1) if replay is not None else ln
        whole = _SERIAL_WHOLE_RE.fullmatch(text)
        if whole is None and _SERIAL_NOISE_RE.fullmatch(text) is None:
            raise HarnessError(f"serial_lines_whole: split line {ln!r}")
        if whole is None or replay is not None:
            continue
        i = int(whole.group(1))
        if i >= SERIAL_WHOLE_N:
            raise HarnessError(f"serial_lines_whole: number out of range: {ln!r}")
        seen[i] += 1
    bad = [i for i, n in enumerate(seen) if n != 1]
    if bad:
        raise HarnessError(
            f"serial_lines_whole: {SERIAL_WHOLE_N - len(bad)} of {SERIAL_WHOLE_N} "
            f"numbered lines whole; line {bad[0]} seen {seen[bad[0]]} times"
        )


# `serial_frame` (DESIGN §2.6): a framed line with `\n`, `\r` and 0x1E inside,
# console bytes from the kernel that leave their line open, then a framed line.
SERIAL_FRAME_OK = "vibeOS: ktest: ok serial_frame"
SERIAL_FRAME_ESCAPED = "vibeOS: ktest: serial frame a?b?c?d"
SERIAL_FRAME_OPEN = "?serial-frame open"
SERIAL_FRAME_AFTER = "vibeOS: ktest: serial frame after open"


def _check_serial_frame(lines: list[str]) -> None:
    """`serial_frame`'s three lines, from raw serial `lines`.

    The first is framed with each of `\n`, `\r` and 0x1E as `?`; the console
    bytes are one unframed line with the 0x1E as `?`; and the last is framed,
    after them, on a line of its own, because the kernel breaks the open user
    line first.
    """
    esc = open_at = after = None
    for i, raw in enumerate(lines):
        framed, text = frame.split_frame(raw)
        if framed and text == SERIAL_FRAME_ESCAPED and esc is None:
            esc = i
        elif not framed and text == SERIAL_FRAME_OPEN and open_at is None:
            open_at = i
        elif framed and text == SERIAL_FRAME_AFTER and after is None:
            after = i
    if esc is None:
        raise HarnessError(f"serial_frame: no framed {SERIAL_FRAME_ESCAPED!r}")
    if open_at is None:
        raise HarnessError(f"serial_frame: no unframed line {SERIAL_FRAME_OPEN!r}")
    if after is None:
        raise HarnessError(f"serial_frame: no framed {SERIAL_FRAME_AFTER!r}")
    if not esc < open_at < after:
        raise HarnessError(
            f"serial_frame: lines out of order ({esc}, {open_at}, {after})"
        )


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
    """One ktest QEMU. It never retries (ROADMAP §10.2, F021).

    A timeout, a `FAIL` line, a panic signature, or a missing marker raises
    `HarnessError` from this one boot.
    """
    raw = run_qemu_until_exit(cfg, timeout_s=timeout)
    results.current().add_boot(qemu_argv(cfg, None), cfg, raw.exit_code)
    klines = frame.kernel_lines(raw.lines)
    results.current().record_ktest_lines(klines)
    check_ktest_output(raw.lines, raw.exit_code)
    if SERIAL_WHOLE_OK in klines:
        _check_serial_whole(klines)
    if SERIAL_FRAME_OK in klines:
        _check_serial_frame(raw.lines)
    _require_line(klines, _block_name("vda"), "missing virtio-blk marker")
    _require_line(klines, _block_name("vdap1"), "missing vdap1 marker")
    if persist_reboot:
        _require_line(
            klines,
            lambda ln: ln == "vibeOS: persist: intact",
            "persist pattern did not survive reboot",
        )
    else:
        _require_line(klines, _block_name("vdap2"), "missing vdap2 marker")
        _require_line(
            klines,
            lambda ln: ln == "vibeOS: persist: wrote",
            "missing persist wrote",
        )
    return raw


def main() -> int:
    env = env_config(default_iso="vibeos-ktest.iso", default_timeout=90)
    results.Results(env.tier)
    skip_persist = env_flag("VIBEOS_SKIP_PERSIST")
    disk = make_disk(DISK_BYTES, "vibeos-vblk-")
    try:
        cfg = env.qemu(extra=ktest_devices(disk, env.smp), boot_order="d")
        try:
            raw = _ktest_boot(cfg, env.timeout, persist_reboot=False)
        except HarnessError as e:
            print(f"[ktest] FAIL: {e}", file=sys.stderr)
            return 1

        klines = frame.kernel_lines(raw.lines)
        oks = [ln for ln in klines if ln.startswith("vibeOS: ktest: ok ")]
        skips = [ln for ln in klines if ln.startswith("vibeOS: ktest: skip ")]
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
    raise SystemExit(results.run_main(main))
