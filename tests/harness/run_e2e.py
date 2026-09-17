#!/usr/bin/env python3
"""Phase 0 e2e runner. Booted by `make test-e2e`."""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import (  # noqa: E402
    PHASE0_GP_MARKERS,
    PHASE0_MARKERS,
    PHASE0_PANIC_PREFIX,
    HarnessError,
    QemuConfig,
    run_qemu_and_check,
)


def main() -> int:
    iso = os.environ.get("VIBEOS_ISO", "vibeos.iso")
    smp = int(os.environ.get("VIBEOS_SMP", "2"))
    cpu = os.environ.get("VIBEOS_QEMU_CPU", "max")
    mem = os.environ.get("VIBEOS_MEM", "128M")
    bios = os.environ.get("VIBEOS_BIOS")  # e.g. path to OVMF_CODE.fd
    expect_panic = os.environ.get("VIBEOS_EXPECT_PANIC", "") not in ("", "0")
    gp_test = os.environ.get("VIBEOS_GP_TEST", "") not in ("", "0")

    cfg = QemuConfig(iso=iso, smp=smp, cpu=cpu, mem=mem, bios=bios)
    if gp_test:
        markers = PHASE0_GP_MARKERS
        expect_panic = True
    elif expect_panic:
        markers = PHASE0_PANIC_PREFIX
    else:
        markers = PHASE0_MARKERS

    try:
        result = run_qemu_and_check(
            cfg,
            markers,
            timeout_s=float(os.environ.get("VIBEOS_TIMEOUT", "60")),
            expect_panic=expect_panic,
        )
    except HarnessError as e:
        print(f"[e2e] FAIL: {e}", file=sys.stderr)
        return 1

    print(f"[e2e] ok: {len(result.matched)} markers matched", file=sys.stderr)
    for name in result.matched:
        print(f"[e2e]   . {name}", file=sys.stderr)
    if expect_panic and result.panic_line:
        print(f"[e2e]   . panic seen: {result.panic_line!r}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
