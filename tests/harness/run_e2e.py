#!/usr/bin/env python3
"""Phase 0 e2e runner. Booted by `make test-e2e`."""

from __future__ import annotations

import os
import sys
from collections.abc import Callable
from typing import TypeVar

from tests.harness.harness import (
    PHASE0_PANIC_PREFIX,
    HarnessError,
    QemuConfig,
    boot_contract_markers,
    run_qemu_and_check,
    run_qemu_console_input,
)

_T = TypeVar("_T")

# Default QEMU `pc` (i440fx) set used by vibeOS e2e. No UHCI unless `-usb`.
PCI_GOLDEN = (
    "8086:1237",  # 440FX host
    "8086:7000",  # PIIX3 ISA
    "8086:7010",  # PIIX3 IDE
    "8086:7113",  # PIIX4 ACPI
    "1234:1111",  # Bochs VGA
    "8086:100e",  # e1000 (QEMU default NIC)
)


def _check_pci_qemu_set(lines: list[str]) -> None:
    """lspci-adjacent boot dump must name the default QEMU `pc` devices."""
    blob = "\n".join(lines)
    missing = [id_ for id_ in PCI_GOLDEN if id_ not in blob]
    if missing:
        raise HarnessError(f"pci dump missing {missing!r}")
    count_line = None
    for line in lines:
        if line.startswith("vibeOS: pci: ") and line.endswith(" devices"):
            count_line = line
            break
    if count_line is None:
        raise HarnessError("pci count marker missing")
    n_s = count_line[len("vibeOS: pci: ") : -len(" devices")]
    try:
        n = int(n_s)
    except ValueError as e:
        raise HarnessError(f"pci count not an int: {count_line!r}") from e
    if n < len(PCI_GOLDEN):
        raise HarnessError(f"pci count {n} < golden {len(PCI_GOLDEN)}")


def _retry_hang(label: str, fn: Callable[[], _T]) -> _T:
    """One silent timeout / missing last marker is a QEMU hang, not a contract fail."""
    try:
        return fn()
    except HarnessError as e:
        msg = str(e)
        if "timed out" not in msg and "no shell ready" not in msg:
            raise
        print(f"[e2e] retry {label}: {e}", file=sys.stderr)
        return fn()


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
            ("  0x", "normal_boot_tail"),
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

    timeout_s = float(os.environ.get("VIBEOS_TIMEOUT", "60"))
    try:
        if expect_panic or gp_test:
            result = run_qemu_and_check(
                cfg,
                markers,
                timeout_s=timeout_s,
                expect_panic=expect_panic,
                dump_needles=dump_needles,
            )
        else:
            result = _retry_hang(
                "marker boot",
                lambda: run_qemu_and_check(
                    cfg,
                    markers,
                    timeout_s=timeout_s,
                    expect_panic=expect_panic,
                    dump_needles=dump_needles,
                ),
            )
    except HarnessError as e:
        print(f"[e2e] FAIL: {e}", file=sys.stderr)
        return 1

    print(f"[e2e] ok: {len(result.matched)} markers matched", file=sys.stderr)
    for name in result.matched:
        print(f"[e2e]   . {name}", file=sys.stderr)
    if expect_panic and result.panic_line:
        print(f"[e2e]   . panic seen: {result.panic_line!r}", file=sys.stderr)
    if not expect_panic and not gp_test:
        try:
            _check_pci_qemu_set(result.lines)
        except HarnessError as e:
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        print("[e2e]   . pci qemu set ok", file=sys.stderr)
        try:
            inp = _retry_hang(
                "console input",
                lambda: run_qemu_console_input(cfg, timeout_s=timeout_s),
            )
        except HarnessError as e:
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        print("[e2e]   . console input serial+ps2 ok", file=sys.stderr)
        for name in inp.matched:
            print(f"[e2e]     . {name}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
