"""Tests for run_pid1.run_case (ROADMAP §10.5, `make test-e2e-init-fault`)."""

from __future__ import annotations

import unittest

from tests.harness import frame
from tests.harness.frame import FRAME
from tests.harness.harness import HarnessError, boot_contract_markers, env_config
from tests.harness.linesource import FakeLineSource
from tests.harness.qmp import FakeQmp
from tests.harness.run_pid1 import CASES, run_case

NAME = "init_fault"


def k(text: str) -> str:
    """A framed kernel line."""
    return FRAME + text


def contract_lines() -> list[str]:
    """One framed line per kernel contract row, in the contract's order."""
    env = env_config(default_iso="fake.iso", default_timeout=60.0)
    out = []
    for m in boot_contract_markers(cpu=env.cpu, smp=env.smp):
        if (m.source or frame.source_of(m.substring)) != frame.KERNEL:
            continue
        out.append(k(" ".join((m.substring, *m.and_contains))))
    return out


# The fault, the kernel's diagnostic (unframed, which a user line can forge),
# then the registered line and the dump.
FAULT = "user: pid 1 killed SIGSEGV rip=0x40001000 err=0x6 cr2=0x1000"
PID1 = k("vibeOS: init: pid 1 killed SIGSEGV addr=0x1000")
DUMP = [
    k("vibeOS: panic:"),
    k("vibeOS: panic: msg: pid 1 exited"),
    k("vibeOS: backtrace:"),
    k("vibeOS: panic: halted"),
]


def run(lines: list[str], *, end: str = "eof", panicked: bool = True) -> None:
    """Run the case over `lines`; the kernel's pvpanic write, QMP's
    `GUEST_PANICKED`, follows the last line unless `panicked` is false."""
    env = env_config(default_iso="fake.iso", default_timeout=60.0)
    src = FakeLineSource.from_lines(lines, end=end, exit_code=0)
    after: dict[int, list[dict[str, object]]] = {
        len(lines): [{"event": "GUEST_PANICKED", "data": {"action": "pause"}}]
    }
    run_case(NAME, env, line_source=src, qmp=FakeQmp([], after_line=after if panicked else None))


class RunCase(unittest.TestCase):
    def test_case_table(self) -> None:
        self.assertEqual(CASES[NAME].variant, "init-fault")

    def test_good_sequence_passes(self) -> None:
        run([*contract_lines(), "hello from ring3", FAULT, PID1, *DUMP])

    def test_unframed_diagnostic_is_not_the_line(self) -> None:
        # Only the unframed `user: pid 1 ... cr2=0x1000` line and an
        # unframed copy of the registered one: the needle is not met.
        with self.assertRaises(HarnessError):
            run([*contract_lines(), FAULT, "vibeOS: init: pid 1 killed SIGSEGV addr=0x1000",
                 *DUMP])

    def test_missing_addr_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "want"):
            run([*contract_lines(), FAULT, k("vibeOS: init: pid 1 killed SIGSEGV"), *DUMP])

    def test_exited_1_fails(self) -> None:
        # The store did not fault, and init_fault exited 1.
        with self.assertRaisesRegex(HarnessError, "exited 1"):
            run([*contract_lines(), k("vibeOS: init: pid 1 exited 1"), *DUMP])

    def test_shell_ready_fails(self) -> None:
        with self.assertRaisesRegex(HarnessError, "working init"):
            run([*contract_lines(), "user: tests ok", "vibeOS: shell ready", FAULT, PID1,
                 *DUMP])

    def test_timeout_fails(self) -> None:
        with self.assertRaises(HarnessError):
            run([*contract_lines(), FAULT], end="timeout", panicked=False)

    def test_no_guest_panicked_fails(self) -> None:
        # The dump ended but the kernel's pvpanic write never arrived.
        with self.assertRaises(HarnessError):
            run([*contract_lines(), FAULT, PID1, *DUMP], end="timeout", panicked=False)

    def test_no_halt_after_line_fails(self) -> None:
        with self.assertRaises(HarnessError):
            run([*contract_lines(), FAULT, PID1, k("vibeOS: panic:")])


if __name__ == "__main__":
    unittest.main()
