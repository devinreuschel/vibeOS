#!/usr/bin/env python3
"""In-guest kernel test runner. DESIGN §8.2. Driven by `make test-kernel`."""

from __future__ import annotations

import os
import sys
import tempfile
from collections.abc import Callable

from tests.harness.harness import (
    HarnessError,
    QemuConfig,
    check_ktest_output,
    effective_accel_name,
    retryable_ktest_failure,
    run_qemu_until_exit,
)

DISK_BYTES = 4 * 1024 * 1024


def _blk_extra(disk: str, smp: int) -> tuple[str, ...]:
    # Boot the ISO, not the virtio disk. Stamping a protective MBR (0x55AA)
    # makes SeaBIOS prefer the HDD on reboot unless CD is first.
    return (
        "-boot",
        "order=d",
        "-drive",
        f"file={disk},if=none,id=vibehd,format=raw,cache=writeback,discard=unmap",
        "-device",
        f"virtio-blk-pci,drive=vibehd,disable-legacy=on,num-queues={smp}",
    )


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


def _ktest_boot(cfg: QemuConfig, timeout: float, *, persist_reboot: bool):
    """One ktest QEMU. Retry once on a known host-timing flake."""
    tag = "persist reboot" if persist_reboot else "ktest"
    last: HarnessError | None = None
    for attempt in range(2):
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
            if attempt == 0 and (
                timed_out
                or retryable
            ):
                reason = "timeout" if timed_out else "known ktest timing flake"
                print(f"[{tag}] retry after {reason}: {e}", file=sys.stderr)
                continue
            raise
    assert last is not None
    raise last


def main() -> int:
    iso = os.environ.get("VIBEOS_ISO", "vibeos-ktest.iso")
    smp = int(os.environ.get("VIBEOS_SMP", "2"))
    cpu = os.environ.get("VIBEOS_QEMU_CPU", "max")
    mem = os.environ.get("VIBEOS_MEM", "128M")
    bios = os.environ.get("VIBEOS_BIOS")
    # ktest-only: e1000e (MSI-X), edu (INTx + DMA), virtio-rng, virtio-blk.
    # e2e stays the default pc device set so `pci: 6 devices` does not move.
    extra = tuple(x for x in os.environ.get("VIBEOS_QEMU_EXTRA", "").split() if x)
    timeout = float(os.environ.get("VIBEOS_TIMEOUT", "90"))
    skip_persist = os.environ.get("VIBEOS_SKIP_PERSIST", "") not in ("", "0")

    fd, disk = tempfile.mkstemp(prefix="vibeos-vblk-", suffix=".img")
    try:
        os.ftruncate(fd, DISK_BYTES)
        os.close(fd)
        fd = -1

        cfg = QemuConfig(
            iso=iso,
            smp=smp,
            cpu=cpu,
            mem=mem,
            bios=bios,
            extra=("-device", "isa-debug-exit,iobase=0xf4,iosize=0x04")
            + ("-device", "e1000e")
            + ("-device", "edu")
            + ("-device", "virtio-rng-pci,disable-legacy=on")
            + _blk_extra(disk, smp)
            + extra,
        )

        try:
            raw = _ktest_boot(cfg, timeout, persist_reboot=False)
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
            raw2 = _ktest_boot(cfg, timeout, persist_reboot=True)
        except HarnessError as e:
            print(f"[ktest] FAIL persist reboot: {e}", file=sys.stderr)
            return 1

        print("[ktest] persist reboot: intact", file=sys.stderr)
        return 0
    finally:
        if fd >= 0:
            os.close(fd)
        try:
            os.unlink(disk)
        except OSError:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
