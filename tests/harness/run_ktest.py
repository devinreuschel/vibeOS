#!/usr/bin/env python3
"""In-guest kernel test runner. DESIGN §8.2. Driven by `make test-kernel`."""

from __future__ import annotations

import dataclasses
import os
import re
import sys
from collections import Counter
from collections.abc import Callable, Iterable

from tests.harness import frame, results, skips
from tests.harness.harness import (
    BOOT_ALLOWANCE_S,
    PANIC_DONE,
    TIMEOUT_SCALE,
    EnvConfig,
    HarnessError,
    KtestDeadlines,
    KtestSummary,
    QemuConfig,
    RunResult,
    check_ktest_output,
    contains_panic,
    default_iso,
    env_config,
    env_flag,
    ktest_devices,
    ktest_lines,
    ktest_summary,
    make_disk,
    parse_ktest_line,
    qemu_argv,
    run_qemu_until_exit,
)

DISK_BYTES = 4 * 1024 * 1024
# The main boot's second virtio-blk disk (`vdb`, 2048 sectors), blank, for
# `block_two_disk_instances`.
DISK2_BYTES = 1 * 1024 * 1024

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


# Kernel stack depth (TESTING.md §8.2, DESIGN §4.5): one line per stack
# size and a report line, just before `vibeOS: ktest: end`.
STACK_MARGIN = 4096
_STACK_LINE_RE = re.compile(
    r"vibeOS: stack: (\d+) used (\d+) of (\d+) by tid (\d+) (\S+)"
)
_STACK_REPORT_RE = re.compile(r"vibeOS: stack: report (\d+) sizes (\d+) lost")


@dataclasses.dataclass(frozen=True)
class StackDepth:
    """The deepest use of one stack size, and the thread that reached it."""

    size: int
    used: int
    tid: int
    name: str

    @property
    def budget(self) -> int:
        """The stack's size minus DESIGN §4.5's 4 KiB margin."""
        return max(self.size - STACK_MARGIN, 0)

    @property
    def over(self) -> bool:
        return self.used > self.budget

    def line(self) -> str:
        return (
            f"vibeOS: stack: {self.size} used {self.used} of {self.budget} "
            f"by tid {self.tid} {self.name}"
        )


@dataclasses.dataclass(frozen=True)
class StackReport:
    """A boot's stack lines: `sizes` and `lost` from the report line
    (`None` when the boot printed none), and each size's deepest use."""

    depths: tuple[StackDepth, ...]
    sizes: int | None
    lost: int | None

    @property
    def over(self) -> tuple[StackDepth, ...]:
        return tuple(d for d in self.depths if d.over)

    def problems(self) -> list[str]:
        """Why the report fails the boot: no report line, a size line
        count that differs from it, a `lost` other than 0, or a use over
        its stack's budget."""
        if self.sizes is None or self.lost is None:
            return ["no `vibeOS: stack: report` line"]
        out: list[str] = []
        if self.sizes != len(self.depths):
            out.append(f"report names {self.sizes} sizes, {len(self.depths)} lines printed")
        if self.lost != 0:
            out.append(f"{self.lost} stack sizes lost")
        out += [
            f"tid {d.tid} {d.name} used {d.used} of {d.size}, over its budget {d.budget}"
            for d in self.over
        ]
        return out


def check_stack_depth(lines: Iterable[str], *, enforce: bool = True) -> StackReport:
    """Parse a boot's stack lines (kernel text, `frame.kernel_lines`).
    With `enforce`, raise `HarnessError` for any of
    `StackReport.problems` (ROADMAP §10.2)."""
    depths: list[StackDepth] = []
    sizes = lost = None
    for ln in lines:
        m = _STACK_LINE_RE.fullmatch(ln)
        if m is not None:
            depths.append(
                StackDepth(int(m.group(1)), int(m.group(2)), int(m.group(4)), m.group(5))
            )
            continue
        r = _STACK_REPORT_RE.fullmatch(ln)
        if r is not None:
            sizes, lost = int(r.group(1)), int(r.group(2))
    report = StackReport(tuple(depths), sizes, lost)
    if enforce:
        bad = report.problems()
        if bad:
            raise HarnessError("stack depth: " + "; ".join(bad))
    return report


def write_stack_summary(label: str, report: StackReport) -> None:
    """Append the boot's stack lines to `$GITHUB_STEP_SUMMARY` when it is
    set: the budget's evidence (ROADMAP §10.2)."""
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    out = [f"### kernel stack depth: {label}", ""]
    out += [f"- `{d.line()}`" + (" **over budget**" if d.over else "") for d in report.depths]
    if report.sizes is None:
        out.append("- no report line")
    else:
        out.append(f"- `vibeOS: stack: report {report.sizes} sizes {report.lost} lost`")
    with open(path, "a", encoding="utf-8") as f:
        f.write("\n".join(out) + "\n\n")


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


def ktest_selection(cfg: QemuConfig) -> str:
    """The `vibeos.ktest=` value the boot gets: `cfg.ktest`, else the last
    such word of its command line (later words win in the kernel)."""
    if cfg.ktest is not None:
        return cfg.ktest
    value = ""
    for word in cfg.cmdline.split():
        if word.startswith("vibeos.ktest="):
            value = word[len("vibeos.ktest=") :]
    return value


def ran(lines: Iterable[str], name: str) -> bool:
    """Whether the boot printed a `run <name>` line."""
    return any(k.kind == "run" and k.name == name for k in ktest_lines(lines))


# `block_persist` writes a pattern the persist reboot rereads (DESIGN §8.2).
PERSIST_TEST = "block_persist"


def _ktest_boot(
    cfg: QemuConfig,
    timeout: float,
    *,
    persist_reboot: bool,
    label: str = "ktest",
    enforce_stack: bool = True,
    scale: float = TIMEOUT_SCALE,
) -> RunResult:
    """One ktest QEMU. It never retries (ROADMAP §10.2, F021).

    `timeout` is the boot allowance and `scale` the timeout scale of the
    progress deadline (`KtestDeadlines`). A timeout, a `FAIL` line, a panic
    signature, a missing marker, a run without its result, a skipped set
    that differs from `tests/harness/skips.toml` (`skips.check_skips`), or a
    stack depth report `check_stack_depth` refuses (unless not
    `enforce_stack`) raises `HarnessError` from this one boot. The persist
    lines are required only when the boot ran `block_persist`. The stack
    lines go to the job summary under `label`.
    """
    raw = run_qemu_until_exit(cfg, timeout_s=timeout, progress=KtestDeadlines(timeout, scale))
    results.current().add_boot(qemu_argv(cfg, None), cfg, raw.exit_code)
    klines = frame.kernel_lines(raw.lines)
    results.current().record_ktest_lines(klines)
    verdict = check_ktest_output(raw.lines, raw.exit_code)
    write_stack_summary(label, check_stack_depth(klines, enforce=False))
    check_stack_depth(klines, enforce=enforce_stack)
    skips.check_skips(
        verdict.ktest_skips,
        verdict.ktest_runs,
        skips.launch_config(cfg),
        skips.load_skips(),
        must_run=skips.must_run_names(ktest_selection(cfg)),
    )
    if SERIAL_WHOLE_OK in klines:
        _check_serial_whole(klines)
    if SERIAL_FRAME_OK in klines:
        _check_serial_frame(raw.lines)
    _require_line(klines, _block_name("vda"), "missing virtio-blk marker")
    _require_line(klines, _block_name("vdap1"), "missing vdap1 marker")
    persist = ran(raw.lines, PERSIST_TEST)
    if persist_reboot:
        if persist:
            _require_line(
                klines,
                lambda ln: ln == "vibeOS: persist: intact",
                "persist pattern did not survive reboot",
            )
    else:
        _require_line(klines, _block_name("vdap2"), "missing vdap2 marker")
        if persist:
            _require_line(
                klines,
                lambda ln: ln == "vibeOS: persist: wrote",
                "missing persist wrote",
            )
    return raw


# The selection proof (ROADMAP §10.2): wildcard rows run on every pass, a
# `.once()` row once, an opt-in row only when named literally, and
# `loglevel=8` reaches `log_boot_level`. `ktest_*` also matches the
# registry's own tests in the sched group.
SELECT_KTEST = "ktest_*,ktest_optin_probe,log_boot_level"
SELECT_REPEAT = 3
SELECT_CMDLINE = "loglevel=8"
SELECT_EXPECTED: dict[str, int] = {
    "ktest_names_unique": 3,
    "ktest_once_probe": 1,
    "ktest_optin_probe": 3,
    "log_boot_level": 3,
    "ktest_rows": 3,
    "ktest_fail_fmt": 3,
    "ktest_helpers": 3,
    "ktest_context": 3,
}
SELECT_ABSENT = frozenset({"ktest_deadline_hang"})
# F074's frame baseline, repeated in one boot at `-smp 2`.
REPEAT_TEST = "reap_many_via_idle"
REPEAT_N = 20


def check_select_run(lines: Iterable[str], expected: dict[str, int], absent: Iterable[str]) -> None:
    """The boot's `begin` count is the sum of `expected`, its `ok` runs are
    exactly `expected` (name to count), and no name in `absent` has a run
    line. Raises `HarnessError`."""
    lines = list(lines)
    s = ktest_summary(lines)
    want_n = sum(expected.values())
    if s.begin != want_n:
        raise HarnessError(f"ktest select: begin {s.begin}, want {want_n}")
    got = Counter(o.name for o in s.oks)
    want = Counter(expected)
    if got != want:
        missing = sorted((want - got).elements())
        extra = sorted((got - want).elements())
        raise HarnessError(f"ktest select: ok runs missing {missing}, extra {extra}")
    ran_absent = sorted({r.name for r in s.runs} & set(absent))
    if ran_absent:
        raise HarnessError(f"ktest select: ran {ran_absent}, which the selection leaves out")


def _proof_boot(
    env: EnvConfig,
    label: str,
    *,
    ktest: str,
    repeat: int | None,
    cmdline: str = "",
    enforce_stack: bool = True,
) -> RunResult:
    """One proof boot on a fresh disk, with its own selection."""
    penv = dataclasses.replace(
        env, ktest=ktest, ktest_repeat=repeat, cmdline=f"{env.cmdline} {cmdline}".strip()
    )
    disk = make_disk(DISK_BYTES, "vibeos-vblk-")
    try:
        cfg = penv.qemu(extra=ktest_devices(disk, env.smp), boot_order="d")
        raw = _ktest_boot(
            cfg,
            env.timeout,
            persist_reboot=False,
            label=f"{env.tier} {label}",
            enforce_stack=enforce_stack,
            scale=env.timeout_scale,
        )
    finally:
        try:
            os.unlink(disk)
        except OSError:
            pass
    print(f"[ktest] {label}:", file=sys.stderr)
    print_ktest_summary(ktest_summary(raw.lines), raw.exit_code)
    return raw


def _record(name: str, ok: bool) -> None:
    results.current().record("marker", name, "passed" if ok else "failed")


def ktest_select_boot(env: EnvConfig) -> None:
    """`vibeos.ktest=` and `vibeos.ktest_repeat=` select and repeat rows
    (ROADMAP §10.2): `SELECT_EXPECTED`'s runs and nothing else."""
    try:
        raw = _proof_boot(
            env, "select", ktest=SELECT_KTEST, repeat=SELECT_REPEAT, cmdline=SELECT_CMDLINE
        )
        check_select_run(raw.lines, SELECT_EXPECTED, SELECT_ABSENT)
    except HarnessError:
        _record("ktest_select_boot", False)
        raise
    _record("ktest_select_boot", True)


def _repeat_boot(env: EnvConfig) -> None:
    """`reap_many_via_idle` 20 times in one boot (ROADMAP §10.2, F074)."""
    raw = _proof_boot(env, "repeat", ktest=REPEAT_TEST, repeat=REPEAT_N)
    check_select_run(raw.lines, {REPEAT_TEST: REPEAT_N}, ())


# The planted stack boot (ROADMAP §10.2): `stack_depth_planted` recurses at
# least 13 KiB on a 16 KiB stack, over its 12 KiB budget.
PLANT_TEST = "stack_depth_planted"
PLANT_THREAD = "stack-plant"


def check_planted(report: StackReport) -> None:
    """The planted boot's verdict: the report is whole, and the one use
    over budget is thread `stack-plant`'s. Raises `HarnessError`."""
    over = [d.name for d in report.over]
    if over != [PLANT_THREAD]:
        raise HarnessError(f"planted stack: over budget {over}, want [{PLANT_THREAD!r}]")
    rest = [p for p in report.problems() if f" {PLANT_THREAD} used " not in p]
    if rest:
        raise HarnessError("planted stack: " + "; ".join(rest))


def _single_test_boot(
    env: EnvConfig, test: str, label: str, *, enforce_stack: bool = True
) -> RunResult:
    """One boot of test `test` alone (`VIBEOS_KTEST`), on a fresh disk."""
    return _proof_boot(env, label, ktest=test, repeat=None, enforce_stack=enforce_stack)


def _planted_boot(env: EnvConfig) -> None:
    """The expect-fail stack boot: `stack_depth_planted` alone, which passes
    only when `check_stack_depth` names `stack-plant` over budget and
    nothing else fails (ROADMAP §10.2)."""
    try:
        raw = _single_test_boot(env, PLANT_TEST, "planted stack", enforce_stack=False)
        check_planted(check_stack_depth(frame.kernel_lines(raw.lines), enforce=False))
    except HarnessError:
        _record(PLANT_TEST, False)
        raise
    _record(PLANT_TEST, True)
    print(f"[ktest] planted stack: {PLANT_THREAD} over budget, as planted", file=sys.stderr)


# The FAT stack boot (ROADMAP §10.4, F058): a disk-backed FAT mount and a
# 64 KiB write on `vda` from a 16 KiB stack, in its own boot on a fresh disk,
# since its image overwrites the GPT `vda` boots with.
FAT_STACK_TEST = "fat_vda_16k_stack"


def _fat_boot(env: EnvConfig) -> None:
    """`fat_vda_16k_stack` alone; the stack check is enforced as in every
    boot, and its `ok` line is required."""
    raw = _single_test_boot(env, FAT_STACK_TEST, "fat 16k stack")
    check_select_run(raw.lines, {FAT_STACK_TEST: 1}, ())


def print_ktest_summary(summary: KtestSummary, exit_code: int | None) -> None:
    """The report on stderr: passes out of runs, the ten slowest runs, the
    info lines, then the exit status."""
    for line in summary.text():
        print(line, file=sys.stderr)
    print(f"[ktest] exit {exit_code}", file=sys.stderr)


# The planted hang (ROADMAP §10.2, T1): IF=0 on CPU 0 with a 500 ms
# deadline, which only another CPU's tick can catch.
TRIP_TEST = "ktest_deadline_hang"
TRIP_DEADLINE_MS = 500
# After `panic: halted` the guest sits in `hlt`: read this long, then kill.
TRIP_KILL_AFTER_S = 0.5


def check_deadline_trip(lines: Iterable[str], name: str, deadline_ms: int) -> None:
    """In order: `run <name> <deadline_ms>`, `FAIL <name>: deadline`, a
    panic signature, then `vibeOS: panic: halted`; and no `ok <name>`.
    Kernel lines only. Raises `HarnessError`."""
    lines = list(lines)
    steps: tuple[tuple[str, Callable[[str], bool]], ...] = (
        (
            f"run {name} {deadline_ms}",
            lambda ln: parse_ktest_line(ln)
            == parse_ktest_line(frame.FRAME + f"vibeOS: ktest: run {name} {deadline_ms}"),
        ),
        (
            f"FAIL {name}: deadline",
            lambda ln: frame.kernel_text(ln) == f"vibeOS: ktest: FAIL {name}: deadline",
        ),
        (
            "a panic signature",
            lambda ln: contains_panic(ln) and frame.kernel_text(ln) != PANIC_DONE,
        ),
        (PANIC_DONE, lambda ln: frame.kernel_text(ln) == PANIC_DONE),
    )
    at = 0
    for what, pred in steps:
        found = next((i for i in range(at, len(lines)) if pred(lines[i])), None)
        if found is None:
            raise HarnessError(f"deadline trip: no {what!r} after line {at}")
        at = found + 1
    for ln in lines:
        k = parse_ktest_line(ln)
        if k is not None and k.kind == "ok" and k.name == name:
            raise HarnessError(f"deadline trip: {name} passed")


def ktest_deadline_trip(env: EnvConfig) -> None:
    """The expect-fail boot of `ktest_deadline_hang` (ROADMAP §10.2, T1):
    another CPU's tick finds its deadline passed, prints the FAIL line and
    panics. Its failure and panic lines are expected (`expect_fail`), and
    `check_deadline_trip` checks them."""
    penv = dataclasses.replace(env, ktest=TRIP_TEST, ktest_repeat=None)
    disk = make_disk(DISK_BYTES, "vibeos-vblk-")
    try:
        cfg = penv.qemu(extra=ktest_devices(disk, env.smp), boot_order="d")
        try:
            raw = run_qemu_until_exit(
                cfg,
                timeout_s=env.timeout,
                kill_after=lambda ln: (
                    TRIP_KILL_AFTER_S if frame.kernel_text(ln) == PANIC_DONE else None
                ),
                expect_fail=True,
                progress=KtestDeadlines(env.timeout, env.timeout_scale),
            )
            results.current().add_boot(qemu_argv(cfg, None), cfg, raw.exit_code)
            check_deadline_trip(raw.lines, TRIP_TEST, TRIP_DEADLINE_MS)
        except HarnessError:
            _record("ktest_deadline_trip", False)
            raise
    finally:
        try:
            os.unlink(disk)
        except OSError:
            pass
    _record("ktest_deadline_trip", True)
    print(f"[ktest] deadline trip: {TRIP_TEST} failed on its deadline", file=sys.stderr)


def main() -> int:
    env = env_config(default_iso=default_iso("ktest"), default_timeout=BOOT_ALLOWANCE_S)
    results.Results(env.tier)
    skip_persist = env_flag("VIBEOS_SKIP_PERSIST")
    disk = make_disk(DISK_BYTES, "vibeos-vblk-")
    disk2 = make_disk(DISK2_BYTES, "vibeos-vblk2-")
    try:
        cfg = env.qemu(
            extra=ktest_devices(disk, env.smp, extra_disks=(disk2,)), boot_order="d"
        )
        try:
            raw = _ktest_boot(
                cfg, env.timeout, persist_reboot=False, label=env.tier, scale=env.timeout_scale
            )
        except HarnessError as e:
            print(f"[ktest] FAIL: {e}", file=sys.stderr)
            return 1
        print_ktest_summary(ktest_summary(raw.lines), raw.exit_code)

        if not skip_persist and ran(raw.lines, PERSIST_TEST):
            try:
                _ktest_boot(
                    cfg,
                    env.timeout,
                    persist_reboot=True,
                    label=f"{env.tier} persist reboot",
                    scale=env.timeout_scale,
                )
            except HarnessError as e:
                print(f"[ktest] FAIL persist reboot: {e}", file=sys.stderr)
                return 1
            print("[ktest] persist reboot: intact", file=sys.stderr)
    finally:
        for path in (disk, disk2):
            try:
                os.unlink(path)
            except OSError:
                pass

    # A boot the user selected with VIBEOS_KTEST is the whole run.
    if env.ktest:
        return 0
    proofs: list[tuple[str, Callable[[EnvConfig], None]]] = [
        ("select boot", ktest_select_boot)
    ]
    if env.smp == 2:
        proofs.append(("repeat boot", _repeat_boot))
    if env.smp >= 2:
        proofs.append(("deadline trip boot", ktest_deadline_trip))
    proofs.append(("planted stack boot", _planted_boot))
    proofs.append(("fat 16k stack boot", _fat_boot))
    for label, proof in proofs:
        try:
            proof(env)
        except HarnessError as e:
            print(f"[ktest] FAIL {label}: {e}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(results.run_main(main))
