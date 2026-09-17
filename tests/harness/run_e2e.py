#!/usr/bin/env python3
"""Phase 0 e2e runner. Booted by `make test-e2e`."""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import (  # noqa: E402
    PHASE0_PANIC_PREFIX,
    HarnessError,
    QemuConfig,
    boot_contract_markers,
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
    expect_pit = os.environ.get("VIBEOS_EXPECT_PIT", "") not in ("", "0")
    extra = tuple(x for x in os.environ.get("VIBEOS_QEMU_EXTRA", "").split() if x)

    if expect_panic or gp_test:
        extra = extra + ("-device", "isa-debug-exit,iobase=0xf4,iosize=0x04")

    cfg = QemuConfig(
        iso=iso,
        smp=smp,
        cpu=cpu,
        mem=mem,
        bios=bios,
        extra=extra,
        hpet=not expect_pit,
    )
    dump_needles: tuple[str | tuple[str, ...], ...] = ()
    if gp_test:
        markers = boot_contract_markers(cpu=cpu, gp=True, smp=smp)
        expect_panic = True
        dump_needles = (
            "#GP",
            "vibeOS: backtrace:",
            "vibeOS: panic: thread",
            ("vibeOS: logrec:", "smp: done"),
            ("  0x", "_start"),
            "vibeOS: panic: halted",
        )
    elif expect_panic:
        markers = PHASE0_PANIC_PREFIX
        dump_needles = (
            "vibeOS: panic: at",
            "intentional panic-test",
            ("vibeOS: logrec:", "serial online"),
            "rust_begin_unwind",
            "vibeOS: panic: halted",
        )
    else:
        markers = boot_contract_markers(cpu=cpu, hpet=not expect_pit, smp=smp)

    try:
        result = run_qemu_and_check(
            cfg,
            markers,
            timeout_s=float(os.environ.get("VIBEOS_TIMEOUT", "60")),
            expect_panic=expect_panic,
            dump_needles=dump_needles,
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
