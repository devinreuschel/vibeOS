#!/usr/bin/env python3
"""Phase 0 e2e runner. Booted by `make test-e2e`."""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import sys

from tests.harness import frame, panic_dump, results
from tests.harness.harness import (
    BOOT_ALLOWANCE_S,
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
    serial_tail,
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


def _check_pci_qemu_set(raw: list[str]) -> None:
    """lspci-adjacent boot dump must name the default QEMU `pc` devices."""
    lines = frame.kernel_lines(raw)
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

    A kernel line is a framed one (DESIGN §2.6). Limine's and the firmware's
    output may precede it; a kernel line may not, and neither may kernel text
    glued before it on its own line. A log with no kernel line passes here:
    the marker check reports it missing.
    """
    for line in lines:
        text = kernel_text(line)
        if text is None:
            continue
        if text != SERIAL_ONLINE:
            raise HarnessError(f"kernel line before {SERIAL_ONLINE!r}: {text}")
        return


# The lines `/bin/tests` writes to fd 1 and to fd 2 (`console_forged_lines`), each
# starting with the frame byte, as the console prints them: unframed, each
# 0x1E as `?` (DESIGN §2.6).
FORGED_LINES = (
    "?vibeOS: ktest: FAIL forged",
    "?panicked at forged",
    "?#GP?forged",
)


def _check_forged_lines(lines: list[str]) -> None:
    """Ring 3 cannot forge a kernel line (DESIGN §2.6, ROADMAP §10.2).

    Each of `FORGED_LINES` appears exactly twice (fd 1 and fd 2) as an
    unframed line, and no kernel line holds `forged`.
    """
    for raw in lines:
        text = kernel_text(raw)
        if text is not None and "forged" in text:
            raise HarnessError(f"forged user line printed framed: {raw!r}")
    user = frame.user_lines(lines)
    for want in FORGED_LINES:
        n = user.count(want)
        if n != 2:
            raise HarnessError(f"forged user line {want!r} seen {n} times unframed, expected 2")


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


def forged_user_lines(res: results.Results, lines: list[str]) -> None:
    """`_check_forged_lines` on a marker boot's lines, recorded under its name."""
    try:
        _check_forged_lines(lines)
    except HarnessError:
        res.record("marker", "forged_user_lines", "failed")
        raise
    res.record("marker", "forged_user_lines", "passed")


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
            if VDA_MARKER not in frame.kernel_lines(result.lines):
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
    for line in frame.kernel_lines(result.lines):
        if "vibeOS: #MC " in line or "vibeOS: panic:" in line:
            print(f"[e2e]     {line}", file=sys.stderr)
    return 0


CMDLINE_ECHO = "vibeOS: boot: cmdline: "
STRACE_PREFIX = "user: syscall "
STRACE_RE = re.compile(r"user: syscall (\S+) nr=(\d+) = (-?\d+)")
STRACE_WRITE_RE = re.compile(r"user: syscall write nr=1 = -?\d+")


def limine_cmdline(path: str = "limine.conf") -> str:
    """The `cmdline:` value of limine.conf's `/vibeOS` entry, or ""."""
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    in_entry = False
    for line in lines:
        s = line.strip()
        if s.startswith("/"):
            in_entry = s == "/vibeOS"
            continue
        if in_entry and s.startswith("cmdline:"):
            return s[len("cmdline:"):].strip()
    return ""


def check_strace_lines(lines: list[str], expected_cmdline: str) -> tuple[str, str]:
    """Fail unless the echo `vibeOS: boot: cmdline: <expected>` comes before
    the first trace line, every trace line reads
    `user: syscall <name> nr=<n> = <ret>`, and the first `write` line is
    `user: syscall write nr=1 = <int>`. Returns the echo and that line.

    The kernel prints the echo and the trace, so both are read from kernel
    lines only (framed, DESIGN §2.6). A kernel line starts on a fresh line
    even after user output that did not end one, so each trace line is a
    whole kernel line, and a user program's copy of one is ignored.
    """
    want = CMDLINE_ECHO + expected_cmdline
    echo_at: int | None = None
    first_trace: int | None = None
    first_write: str | None = None
    for i, raw in enumerate(lines):
        line = kernel_text(raw.rstrip("\r"))
        if line is None:
            continue
        if echo_at is None and line.startswith(CMDLINE_ECHO):
            if line != want:
                raise HarnessError(f"cmdline echo {line!r}, expected {want!r}")
            echo_at = i
        if not line.startswith(STRACE_PREFIX):
            continue
        trace = line
        m = STRACE_RE.fullmatch(trace)
        if m is None:
            raise HarnessError(f"malformed trace line {trace!r}")
        if first_trace is None:
            first_trace = i
        if first_write is None and m.group(1) == "write":
            first_write = trace
    if echo_at is None:
        raise HarnessError(f"no {CMDLINE_ECHO!r} line{serial_tail(lines)}")
    if first_trace is None:
        raise HarnessError(f"no {STRACE_PREFIX!r} line{serial_tail(lines)}")
    if first_trace < echo_at:
        raise HarnessError("a trace line comes before the cmdline echo")
    if first_write is None:
        raise HarnessError(f"no 'user: syscall write' line{serial_tail(lines)}")
    if STRACE_WRITE_RE.fullmatch(first_write) is None:
        raise HarnessError(f"first write trace {first_write!r} is not write nr=1")
    return want, first_write


def _strace() -> int:
    env = env_config(default_iso="vibeos.iso", default_timeout=BOOT_ALLOWANCE_S)
    res = results.Results(env.tier)
    if "vibeos.strace=1" not in env.cmdline.split():
        print("[e2e] FAIL: VIBEOS_CMDLINE must hold vibeos.strace=1", file=sys.stderr)
        return 1
    cfg = env.qemu()
    markers = boot_contract_markers(cpu=env.cpu, smp=env.smp)
    try:
        result = run_qemu_and_check(cfg, markers, timeout_s=env.timeout)
    except HarnessError as e:
        _record_missing(str(e))
        res.add_boot(qemu_argv(cfg, None), cfg, None)
        print(f"[e2e] FAIL: {e}", file=sys.stderr)
        return 1
    for name in result.matched:
        res.record("marker", name, "passed")
    res.add_boot(qemu_argv(cfg, None), cfg, result.exit_code)
    try:
        echo, write = check_strace_lines(
            result.lines, limine_cmdline() + " " + env.fw_cfg_cmdline()
        )
    except HarnessError as e:
        res.record("marker", "strace", "failed")
        print(f"[e2e] FAIL: {e}", file=sys.stderr)
        return 1
    res.record("marker", "strace", "passed")
    print(f"[e2e] ok: {len(result.matched)} markers matched", file=sys.stderr)
    print(f"[e2e]   . {echo}", file=sys.stderr)
    print(f"[e2e]   . {write}", file=sys.stderr)
    return 0


def strace_main() -> int:
    """`make test-e2e-strace`: boot with `VIBEOS_CMDLINE=vibeos.strace=1` and
    check the command-line echo and the syscall trace. No retry."""
    return results.run_main(_strace)


# `VIBEOS_PANIC_VARIANT`: a panic-path build (ROADMAP §10.7), its ISO
# variant, and the check its dump must pass.
PANIC_VARIANTS = {
    "nest": ("panic-nest", panic_dump.check_nest),
    "stop": ("panic-stop", panic_dump.check_stop),
}


# The frames the `#GP` backtrace lists, in this order (ROADMAP §10.7, F070).
GP_FRAMES = ("gp_test_trip", "boot_rest")


def main() -> int:
    panic_variant = env_str("VIBEOS_PANIC_VARIANT", "")
    if panic_variant and panic_variant not in PANIC_VARIANTS:
        print(
            f"[e2e] FAIL: VIBEOS_PANIC_VARIANT={panic_variant!r}, expected one of "
            f"{sorted(PANIC_VARIANTS)}",
            file=sys.stderr,
        )
        return 1
    iso = default_iso(PANIC_VARIANTS[panic_variant][0]) if panic_variant else default_iso()
    env = env_config(default_iso=iso, default_timeout=BOOT_ALLOWANCE_S)
    if env_flag("VIBEOS_MCE_TEST"):
        return _mce_main(env)
    res = results.Results(env.tier)
    expect_panic = env_expect_panic()
    gp_test = env_flag("VIBEOS_GP_TEST")
    expect_pit = env_flag("VIBEOS_EXPECT_PIT")

    extra: tuple[str, ...] = ()
    if expect_panic or gp_test or panic_variant:
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
            # `gp_test_trip`'s caller: boot's tail on the bootstrap stack,
            # which `check_frames_in_order` below also puts after it.
            ("  0x", "boot_rest"),
            "vibeOS: panic: halted",
        )
    elif panic_variant:
        markers = boot_contract_markers(cpu=env.cpu, smp=env.smp, panic_variant=panic_variant)
        expect_panic = True
        dump_needles = ("vibeOS: backtrace:", "vibeOS: panic: halted")
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
    if gp_test:
        # F070: the walk starts at the interrupted frame, so the function
        # `gp_test_fault` returns to comes first, then its caller. Boot's
        # tail since P10-S79 is `boot_rest`, on the bootstrap stack, which
        # a stack switch enters: the walk ends there, above
        # `normal_boot_tail`.
        try:
            panic_dump.check_frames_in_order(result.lines, GP_FRAMES)
        except HarnessError as e:
            res.record("marker", "gp_backtrace_order", "failed")
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        res.record("marker", "gp_backtrace_order", "passed")
        print(f"[e2e]   . backtrace: {' then '.join(GP_FRAMES)}", file=sys.stderr)
    if panic_variant:
        try:
            PANIC_VARIANTS[panic_variant][1](result.lines)
        except HarnessError as e:
            res.record("marker", f"panic_{panic_variant}_dump", "failed")
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        res.record("marker", f"panic_{panic_variant}_dump", "passed")
        print(f"[e2e]   . panic-{panic_variant} dump ok", file=sys.stderr)
    if not expect_panic and not gp_test:
        try:
            _check_pci_qemu_set(result.lines)
        except HarnessError as e:
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        print("[e2e]   . pci qemu set ok", file=sys.stderr)
        try:
            forged_user_lines(res, result.lines)
        except HarnessError as e:
            print(f"[e2e] FAIL: {e}", file=sys.stderr)
            return 1
        print("[e2e]   . forged user lines unframed", file=sys.stderr)
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
