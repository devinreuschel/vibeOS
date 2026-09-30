"""Unit tests for tests/harness/panic_dump.py (ROADMAP §10.7)."""

from __future__ import annotations

import unittest

from tests.harness import panic_dump
from tests.harness.frame import FRAME
from tests.harness.harness import HarnessError


def k(*texts: str) -> list[str]:
    """Kernel (framed) lines."""
    return [FRAME + t for t in texts]


BOOT = k("vibeOS: serial online", "vibeOS: boot: panic-nest armed")
NEST_DUMP = k(
    "vibeOS: panic:",
    "vibeOS: panic: at src/smp/per_cpu_init.rs:322:9",
    "vibeOS: panic: msg: irq nest underflow",
    "vibeOS: regs: rbp=0x1 rsp=0x2 rflags=0x2 rip=0x3 cr3=0x4",
    "vibeOS: panic: thread cpu=0 tid=0 bootstrap",
    "vibeOS: log: last 1 (0 dropped, 0 sink, 0 reentry)",
    "vibeOS: logrec: 5ms cpu0 info vibeOS: boot: panic-nest armed",
    "vibeOS: backtrace:",
    "  0xffffffff80001000 vibeos::smp::per_cpu_init::irq_nest_leave",
    "vibeOS: panic: halted",
)


class TestCheckNest(unittest.TestCase):
    def test_good_dump_passes(self) -> None:
        panic_dump.check_nest(BOOT + NEST_DUMP)

    def test_reentered_fails(self) -> None:
        bad = BOOT + NEST_DUMP[:3] + k("vibeOS: panic: reentered") + NEST_DUMP[3:]
        with self.assertRaisesRegex(HarnessError, "re-entered"):
            panic_dump.check_nest(bad)

    def test_other_message_fails(self) -> None:
        bad = [ln.replace("irq nest underflow", "something else") for ln in BOOT + NEST_DUMP]
        with self.assertRaisesRegex(HarnessError, "irq nest underflow"):
            panic_dump.check_nest(bad)

    def test_two_banners_fail(self) -> None:
        with self.assertRaisesRegex(HarnessError, "2 dump banners"):
            panic_dump.check_nest(BOOT + NEST_DUMP + k("vibeOS: panic:"))

    def test_two_halted_fail(self) -> None:
        with self.assertRaisesRegex(HarnessError, "halted"):
            panic_dump.check_nest(BOOT + NEST_DUMP + k("vibeOS: panic: halted"))

    def test_user_copy_does_not_count(self) -> None:
        # A process printing dump text unframed changes nothing.
        panic_dump.check_nest(BOOT + ["vibeOS: panic:", "vibeOS: panic: reentered"] + NEST_DUMP)

    def test_no_message_fails(self) -> None:
        bad = [ln for ln in BOOT + NEST_DUMP if "msg:" not in ln]
        with self.assertRaisesRegex(HarnessError, "msg"):
            panic_dump.check_nest(bad)


if __name__ == "__main__":
    unittest.main()
