"""The utest verdict: `/bin/tests`' protocol in an e2e boot (DESIGN §8.2,
ROADMAP §10.5, C-USERTESTS).

`/bin/tests` prints the ktest protocol's form with `utest:` as unframed
user lines (`user/src/utest.rs`): `begin <n>`, `run <name> <deadline_ms>`
before each case, one `ok <name>`, `FAIL <name>: <why>` or `skip <name>:
<reason>` per run, and `end`; an `info <name>: <text>` line is a
case's detail, never a result. Each e2e boot that reaches `shell ready`
feeds every serial line to a `UtestVerdict` (`run_qemu_and_check`'s and
`run_qemu_console_input`'s `utest` parameter), which counts the runs with
the ktest verdict's `RunCounter`, gives them `KtestDeadlines`' progress
deadline (the only deadline a user test has: nothing in the guest enforces
one), compares the skips with `skips.toml`'s matching rows (C-SKIPS), and
records each verdict in the results file under `utest` (C-RESULTS).

Only unframed lines count (C-FRAME): a kernel line holding the same text
is not `/bin/tests`' line. An unframed line that starts with the prefix but
does not parse fails the boot.
"""

from __future__ import annotations

import time
from collections.abc import Mapping

from tests.harness import frame, results, skips
from tests.harness.harness import (
    BOOT_ALLOWANCE_S,
    KTEST_MESSAGES,
    TIMEOUT_SCALE,
    HarnessError,
    KtestDeadlines,
    KtestLine,
    Protocol,
    QemuConfig,
    RunCounter,
    parse_ktest_line,
)

UTEST_PREFIX = "vibeOS: utest: "
UTEST = Protocol("utest", UTEST_PREFIX, frame.USER)

# `RunCounter`'s failures, worded for user tests.
UTEST_MESSAGES: Mapping[str, str] = {
    **KTEST_MESSAGES,
    "begin_after_end": "utest begin after end",
    "second_begin": "utest: a second begin",
    "begin_no_count": "utest begin without a run count",
    "begin_zero": "utest: begin 0",
    "run_outside": "utest run {name} outside begin and end",
    "run_open": "utest run {open} has no result (next: run {name})",
    "result_no_run": "utest result for {name} with no open run: {kind}",
    "result_other": "utest run {open} got a result for {name}",
    "end_no_begin": "utest end without begin",
    "end_open": "utest run {open} has no result before end",
    "count": "utest count {results} != begin {n} ({runs} runs)",
}


class UtestVerdict:
    """One boot's utest verdict. `feed` each raw serial line; call `finish`
    at the boot's last marker. Both raise `HarnessError` on a failure.

    `cfg` is the boot's configuration, which picks the `skips.toml` rows
    (`skips` replaces the file's rows); `allowance` bounds the stretch from
    `end` to the boot's next line that ends a stretch, and `scale`
    multiplies the progress deadlines (`EnvConfig.timeout_scale`).
    """

    def __init__(
        self,
        cfg: QemuConfig,
        rows: list[skips.SkipRow] | None = None,
        *,
        allowance: float = BOOT_ALLOWANCE_S,
        scale: float = TIMEOUT_SCALE,
    ) -> None:
        self.config = skips.launch_config(cfg)
        self.rows = skips.load_skips() if rows is None else rows
        self.count = RunCounter(UTEST_MESSAGES)
        self.deadlines = KtestDeadlines(allowance, scale, UTEST)
        self.expected: dict[str, set[str]] = {}
        for row in self.rows:
            if skips.row_matches(row, self.config):
                self.expected.setdefault(row.name, set()).add(row.reason)
        self.passed: list[str] = []

    def running(self) -> bool:
        """Whether the boot is between `begin` and `end`."""
        return self.deadlines.stretch == "tests"

    def hung_message(self) -> str:
        """What timed out, naming the open or last run (`utest hung in <name>`)."""
        return self.deadlines.hung_message()

    def feed(self, line: str, now: float | None = None) -> float | None:
        """Read raw serial `line`, seen at `now` (`time.monotonic()` by
        default): the boot's new `time.monotonic()` deadline when the line
        is a protocol line, else None."""
        text = frame.user_text(line)
        if text is None or not text.startswith(UTEST_PREFIX):
            return None
        k = parse_ktest_line(line, UTEST)
        if k is None or k.kind == "bad_option":
            raise HarnessError(f"utest: malformed line {text!r}")
        if k.kind == "info":
            return None
        self.count.feed(k)
        self._result(k)
        return self.deadlines.on_line(line, time.monotonic() if now is None else now)

    def _result(self, k: KtestLine) -> None:
        res = results.current()
        if k.kind == "fail":
            res.record("utest", k.name, "failed")
            raise HarnessError(f"utest FAIL: {k.name}: {k.text}")
        if k.kind == "skip":
            want = self.expected.get(k.name)
            if want is None:
                res.record("utest", k.name, "failed")
                raise HarnessError(f"utest skip {k.name} not in skips.toml ({k.text!r})")
            if k.text not in want:
                res.record("utest", k.name, "failed")
                raise HarnessError(
                    f"utest skip {k.name} not in skips.toml: reason {k.text!r}, "
                    f"the rows say {' or '.join(repr(r) for r in sorted(want))}"
                )
            res.record("utest", k.name, "skipped")
        elif k.kind == "ok":
            if k.name in self.expected:
                res.record("utest", k.name, "failed")
                raise HarnessError(f"utest {k.name} ran but skips.toml lists it")
            res.record("utest", k.name, "passed")
            self.passed.append(k.name)

    def finish(self, before: str) -> None:
        """The boot reached marker `before`: `begin` and `end` were seen,
        and `begin`'s count of runs, each with its result."""
        c = self.count
        if c.n is None:
            raise HarnessError(f"no utest begin before {before}")
        if not c.saw_end:
            if c.open_run is not None:
                raise HarnessError(f"utest run {c.open_run} has no result before {before}")
            raise HarnessError(f"no utest end before {before}")
        c.check_count()

    def summary(self) -> str:
        """`<n> run, <k> skipped`."""
        return f"{len(self.count.runs)} run, {len(self.count.skips)} skipped"
