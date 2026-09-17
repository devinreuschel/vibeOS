#!/usr/bin/env python3
"""In-guest kernel test runner. DESIGN §8.2. Driven by `make test-kernel`."""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "harness"))

from harness import (  # noqa: E402
    HarnessError,
    QemuConfig,
    check_ktest_output,
    run_qemu_until_exit,
)


def main() -> int:
    iso = os.environ.get("VIBEOS_ISO", "vibeos-ktest.iso")
    smp = int(os.environ.get("VIBEOS_SMP", "2"))
    cpu = os.environ.get("VIBEOS_QEMU_CPU", "max")
    mem = os.environ.get("VIBEOS_MEM", "128M")
    bios = os.environ.get("VIBEOS_BIOS")
    extra = tuple(os.environ.get("VIBEOS_QEMU_EXTRA", "").split())
    timeout = float(os.environ.get("VIBEOS_TIMEOUT", "90"))

    cfg = QemuConfig(
        iso=iso,
        smp=smp,
        cpu=cpu,
        mem=mem,
        bios=bios,
        extra=("-device", "isa-debug-exit,iobase=0xf4,iosize=0x04") + extra,
    )

    try:
        raw = run_qemu_until_exit(cfg, timeout_s=timeout)
        check_ktest_output(raw.lines, raw.exit_code)
    except HarnessError as e:
        print(f"[ktest] FAIL: {e}", file=sys.stderr)
        return 1

    oks = [ln for ln in raw.lines if "vibeOS: ktest: ok " in ln]
    skips = [ln for ln in raw.lines if "vibeOS: ktest: skip " in ln]
    print(
        f"[ktest] ok: {len(oks)} passed, {len(skips)} skipped, exit {raw.exit_code}",
        file=sys.stderr,
    )
    for ln in oks + skips:
        print(f"[ktest]   . {ln}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
