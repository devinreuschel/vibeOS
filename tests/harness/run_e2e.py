#!/usr/bin/env python3
"""Phase 0 e2e runner. Booted by `make test-e2e`."""

from __future__ import annotations

import sys
from collections.abc import Callable
from typing import TypeVar

from tests.harness import results
from tests.harness.harness import (
    MCE_MCG_STATUS,
    MCE_UC_STATUS,
    EnvConfig,
    HarnessError,
    boot_contract_markers,
    env_config,
    env_expect_panic,
    env_flag,
    halt_test_markers,
    mce_monitor_cmd,
    qemu_argv,
    run_qemu_and_check,
    run_qemu_console_input,
    run_qemu_inject_mce,
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

ISA_DEBUG_EXIT = ("-device", "isa-debug-exit,iobase=0xf4,iosize=0x04")


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
        results.current().retry(label, results.failure_line(msg))
        _record_missing(msg)
        return fn()


def _record_missing(message: str) -> None:
    name = results.missing_marker(message)
    if name is not None:
        results.current().record("marker", name, "failed")


def _mce_main(env: EnvConfig) -> int:
    """Boot, inject an uncorrected machine check on CPU 0, expect dump and halt."""
    results.Results(env.tier)
    cfg = env.qemu()
    markers = boot_contract_markers(cpu=env.cpu, smp=env.smp)
    cmd = mce_monitor_cmd(
        cpu=0, bank=1, status=MCE_UC_STATUS, mcg_status=MCE_MCG_STATUS
    )
    try:
        result = run_qemu_inject_mce(
            cfg, markers, cmd=cmd, timeout_s=env.timeout
        )
    except HarnessError as e:
        _record_missing(str(e))
        if results.missing_marker(str(e)) is None:
            results.current().record("marker", "mce_dump", "failed")
        results.current().add_boot(qemu_argv(cfg, None), cfg, None)
        print(f"[e2e] FAIL: {e}", file=sys.stderr)
        return 1
    for name in result.matched:
        results.current().record("marker", name, "passed")
    results.current().record("marker", "mce_dump", "passed")
    results.current().add_boot(qemu_argv(cfg, None), cfg, result.exit_code)
    print(f"[e2e] ok: {len(result.matched)} markers matched", file=sys.stderr)
    print(f"[e2e]   . {cmd}: #MC dump and halt", file=sys.stderr)
    return 0


def main() -> int:
    env = env_config(default_iso="vibeos.iso", default_timeout=60)
    if env_flag("VIBEOS_MCE_TEST"):
        return _mce_main(env)
    res = results.Results(env.tier)
    expect_panic = env_expect_panic()
    gp_test = env_flag("VIBEOS_GP_TEST")
    expect_pit = env_flag("VIBEOS_EXPECT_PIT")

    extra: tuple[str, ...] = ()
    if expect_panic or gp_test:
        extra = ISA_DEBUG_EXIT

    cfg = env.qemu(extra=extra, hpet=not expect_pit)
    dump_needles: tuple[str | tuple[str, ...], ...] = ()
    if gp_test:
        markers = boot_contract_markers(cpu=env.cpu, gp=True, smp=env.smp)
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
        markers = halt_test_markers()
        dump_needles = (
            "vibeOS: panic: at",
            "intentional panic-test",
            ("vibeOS: logrec:", "serial online"),
            "rust_begin_unwind",
            "vibeOS: panic: halted",
        )
    else:
        markers = boot_contract_markers(
            cpu=env.cpu, hpet=not expect_pit, smp=env.smp
        )

    try:
        if expect_panic or gp_test:
            result = run_qemu_and_check(
                cfg,
                markers,
                timeout_s=env.timeout,
                expect_panic=expect_panic,
                dump_needles=dump_needles,
            )
        else:
            result = _retry_hang(
                "marker boot",
                lambda: run_qemu_and_check(
                    cfg,
                    markers,
                    timeout_s=env.timeout,
                    expect_panic=expect_panic,
                    dump_needles=dump_needles,
                ),
            )
    except HarnessError as e:
        _record_missing(str(e))
        res.add_boot(qemu_argv(cfg, None), cfg, None)
        print(f"[e2e] FAIL: {e}", file=sys.stderr)
        return 1
    for name in result.matched:
        res.record("marker", name, "passed")
    res.add_boot(qemu_argv(cfg, None), cfg, result.exit_code)

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
                lambda: run_qemu_console_input(cfg, timeout_s=env.timeout),
            )
        except HarnessError as e:
            _record_missing(str(e))
            res.add_boot(qemu_argv(cfg, None), cfg, None)
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        for name in inp.matched:
            res.record("marker", name, "passed")
        res.add_boot(qemu_argv(cfg, None), cfg, inp.exit_code)
        print("[e2e]   . console input serial+ps2 ok", file=sys.stderr)
        for name in inp.matched:
            print(f"[e2e]     . {name}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(results.run_main(main))
