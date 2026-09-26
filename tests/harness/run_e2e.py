#!/usr/bin/env python3
"""Phase 0 e2e runner. Booted by `make test-e2e`."""

from __future__ import annotations

import hashlib
import os
import subprocess
import sys
from collections.abc import Callable
from typing import TypeVar

from tests.harness import results
from tests.harness.harness import (
    EnvConfig,
    HarnessError,
    boot_contract_markers,
    env_config,
    env_expect_panic,
    env_flag,
    env_str,
    halt_test_markers,
    make_disk,
    qemu_argv,
    run_qemu_and_check,
    run_qemu_console_input,
    virtio_blk_args,
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


VDA_BYTES = 1 << 20
VDA_MARKER = "vibeOS: block: vda 2048 sectors"


def _sha256(path: str) -> str:
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def _vda_image(case: str, path: str) -> None:
    if case == "vda_vibefs_untouched":
        mkfs = env_str("VIBEOS_MKFS", "mkfs-vibefs")
        r = subprocess.run([mkfs, "-L", "vda", path], capture_output=True, text=True)
        if r.returncode != 0:
            raise HarnessError(f"{case}: mkfs-vibefs failed: {r.stderr or r.stdout}")
    else:
        # The entry-less MBR `part` also finds on a whole-disk FAT32 volume.
        with open(path, "r+b") as f:
            f.seek(510)
            f.write(b"\x55\xaa")


def _check_vda_untouched(env: EnvConfig) -> None:
    """The production kernel writes no byte of a `vda` it did not format (F003).

    Boots the production ISO once per image through `shell ready` and compares
    the image's SHA-256 before and after. No retry: the standing gate forbids one.
    """
    res = results.current()
    for case in ("vda_vibefs_untouched", "vda_55aa_untouched"):
        disk = make_disk(VDA_BYTES, "vibeos-vda-")
        try:
            _vda_image(case, disk)
            before = _sha256(disk)
            # SeaBIOS would try to boot the 0x55AA disk.
            cfg = env.qemu(extra=virtio_blk_args(disk, env.smp), boot_order="d")
            try:
                result = run_qemu_and_check(
                    cfg,
                    boot_contract_markers(cpu=env.cpu, smp=env.smp),
                    timeout_s=env.timeout,
                )
            except HarnessError as e:
                _record_missing(str(e))
                res.add_boot(qemu_argv(cfg, None), cfg, None)
                res.record("marker", case, "failed")
                raise HarnessError(f"{case}: {e}") from e
            res.add_boot(qemu_argv(cfg, None), cfg, result.exit_code)
            if VDA_MARKER not in result.lines:
                res.record("marker", case, "failed")
                raise HarnessError(f"{case}: no {VDA_MARKER!r}")
            after = _sha256(disk)
            if after != before:
                res.record("marker", case, "failed")
                raise HarnessError(f"{case}: vda sha256 changed {before} -> {after}")
            res.record("marker", case, "passed")
            print(f"[e2e]   . {case} ok", file=sys.stderr)
        finally:
            try:
                os.unlink(disk)
            except OSError:
                pass


def main() -> int:
    env = env_config(default_iso="vibeos.iso", default_timeout=60)
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
        if env.tier == "test-e2e":
            try:
                _check_vda_untouched(env)
            except HarnessError as e:
                print(f"[e2e] FAIL: {e}", file=sys.stderr)
                return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(results.run_main(main))
