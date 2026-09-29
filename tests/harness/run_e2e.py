#!/usr/bin/env python3
"""Phase 0 e2e runner. Booted by `make test-e2e`."""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import sys

from tests.harness import results
from tests.harness.harness import (
    KERNEL_LINE_PREFIX,
    MCE_MCG_STATUS,
    MCE_UC_STATUS,
    SERIAL_ONLINE,
    EnvConfig,
    HarnessError,
    boot_contract_markers,
    default_iso,
    env_config,
    env_expect_panic,
    env_flag,
    env_str,
    halt_test_markers,
    kernel_text,
    make_disk,
    mce_monitor_cmd,
    qemu_argv,
    run_qemu_and_check,
    run_qemu_console_input,
    run_qemu_inject_mce,
    virtio_blk_args,
)

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


def check_first_kernel_line(lines: list[str]) -> None:
    """`vibeOS: serial online` is the kernel's first serial line (DESIGN §8.3).

    Limine's and the firmware's output may precede it; a kernel line may not,
    and neither may kernel text glued before it on its own line. A log with no
    serial line passes here: the marker check reports it missing.
    """
    for line in lines:
        at = line.find(SERIAL_ONLINE)
        if at >= 0:
            if KERNEL_LINE_PREFIX in line[:at]:
                raise HarnessError(f"kernel line before {SERIAL_ONLINE!r}: {line}")
            return
        if kernel_text(line) is not None:
            raise HarnessError(f"kernel line before {SERIAL_ONLINE!r}: {line}")


# The boot log's memory diagnostics (DESIGN §8.3).
PMM_FREE_RE = re.compile(r"^vibeOS: pmm: (\d+) free 4KiB frames$")
PMM_TOTAL_RE = re.compile(r"^vibeOS: pmm: (\d+) total, largest order (-?\d+)$")
MEMINFO_TOTAL_RE = re.compile(r"^vibeOS: meminfo: total (\d+) frames, free (\d+), used (\d+)\b")
MEMINFO_HEAP_RE = re.compile(r"^vibeOS: meminfo: heap used (\d+) B / capacity (\d+) B$")
MEMINFO_PREFIX = "vibeOS: meminfo: "


def check_meminfo(lines: list[str]) -> None:
    """`diag::meminfo`'s boot lines agree with the `pmm:` lines (ROADMAP §10.2).

    Each `meminfo:` line (grouped by its text up to the first digit) and each
    `pmm:` line appears once; the frame total equals pmm's; free is at most
    pmm's free count; used is total minus free; heap use is at most capacity.
    """
    texts = [t for t in (kernel_text(ln) for ln in lines) if t is not None]
    groups: dict[str, int] = {}
    for t in texts:
        if t.startswith(MEMINFO_PREFIX):
            key = re.match(r"\D*", t)
            k = key.group() if key else t
            groups[k] = groups.get(k, 0) + 1
    for k, n in groups.items():
        if n != 1:
            raise HarnessError(f"meminfo: {k!r} line appears {n} times, expected once")

    def once(rx: re.Pattern[str], what: str) -> re.Match[str]:
        found = [m for m in (rx.match(t) for t in texts) if m is not None]
        if len(found) != 1:
            raise HarnessError(f"meminfo: {len(found)} {what!r} lines, expected one")
        return found[0]

    pmm_free = int(once(PMM_FREE_RE, "vibeOS: pmm: <n> free 4KiB frames").group(1))
    pmm_total = int(once(PMM_TOTAL_RE, "vibeOS: pmm: <n> total").group(1))
    m = once(MEMINFO_TOTAL_RE, "vibeOS: meminfo: total")
    total, free, used = int(m.group(1)), int(m.group(2)), int(m.group(3))
    h = once(MEMINFO_HEAP_RE, "vibeOS: meminfo: heap used")
    heap_used, heap_cap = int(h.group(1)), int(h.group(2))
    if total != pmm_total:
        raise HarnessError(f"meminfo: total {total} frames, but pmm: {pmm_total} total")
    if free > pmm_free:
        raise HarnessError(f"meminfo: free {free} above pmm: {pmm_free} free 4KiB frames")
    if used != total - free:
        raise HarnessError(f"meminfo: used {used} is not total {total} minus free {free}")
    if heap_used > heap_cap:
        raise HarnessError(f"meminfo: heap used {heap_used} B above capacity {heap_cap} B")


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
    try:
        check_first_kernel_line(result.lines)
    except HarnessError as e:
        print(f"[e2e] FAIL: {e}", file=sys.stderr)
        return 1
    for name in result.matched:
        results.current().record("marker", name, "passed")
    results.current().record("marker", "mce_dump", "passed")
    results.current().add_boot(qemu_argv(cfg, None), cfg, result.exit_code)
    print(f"[e2e] ok: {len(result.matched)} markers matched", file=sys.stderr)
    print(f"[e2e]   . {cmd}: #MC dump and halt", file=sys.stderr)
    for line in result.lines:
        if "vibeOS: #MC " in line or "vibeOS: panic:" in line:
            print(f"[e2e]     {line}", file=sys.stderr)
    return 0


def main() -> int:
    env = env_config(default_iso=default_iso(), default_timeout=60)
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
            # The symbolized frame the ksyms table names right only when the
            # second link leaves .text in place (ROADMAP §10.2, F084).
            ("  0x", "core::panicking::panic_fmt"),
            "vibeOS: panic: halted",
        )
    else:
        markers = boot_contract_markers(
            cpu=env.cpu, hpet=not expect_pit, smp=env.smp
        )

    try:
        result = run_qemu_and_check(
            cfg,
            markers,
            timeout_s=env.timeout,
            expect_panic=expect_panic,
            dump_needles=dump_needles,
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
        print(f"[e2e]   . panic exit status {result.exit_code}", file=sys.stderr)
    try:
        check_first_kernel_line(result.lines)
    except HarnessError as e:
        print(f"[e2e] FAIL: {e}", file=sys.stderr)
        return 1
    print("[e2e]   . serial online is the first kernel line", file=sys.stderr)
    if not expect_panic and not gp_test:
        try:
            _check_pci_qemu_set(result.lines)
        except HarnessError as e:
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        print("[e2e]   . pci qemu set ok", file=sys.stderr)
        try:
            check_meminfo(result.lines)
        except HarnessError as e:
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        print("[e2e]   . meminfo ok", file=sys.stderr)
        try:
            inp = run_qemu_console_input(cfg, timeout_s=env.timeout)
        except HarnessError as e:
            _record_missing(str(e))
            res.add_boot(qemu_argv(cfg, None), cfg, None)
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        for name in inp.matched:
            res.record("marker", name, "passed")
        res.add_boot(qemu_argv(cfg, None), cfg, inp.exit_code)
        try:
            check_first_kernel_line(inp.lines)
        except HarnessError as e:
            print(f"[e2e] FAIL: console boot: {e}", file=sys.stderr)
            return 1
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
