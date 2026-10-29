"""aarch64 MMU bring-up sequences (#260).

The secondary stub invalidates the local TLB after the HCR_EL2 E2H/TGE
write and again after the TTBR writes, before the isb that publishes
SCTLR.M. The boot CPU installs TTBR1 through a reserved empty root with
a local TLBI between the two writes, from a TTBR0 identity map.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SECONDARY = ROOT / "src/arch/aarch64/secondary.rs"
MMU = ROOT / "src/arch/aarch64/mmu.rs"


def global_asm(text: str) -> list[str]:
    """Instruction lines of every `global_asm!` block, in order."""
    lines: list[str] = []
    for block in re.findall(r"global_asm!\((.*?)\);", text, re.S):
        for raw in re.findall(r'"([^"]*)"', block):
            line = raw.strip()
            if not line or line.startswith((".", "//")) or line.endswith(":"):
                continue
            lines.append(line)
    return lines


def find_line(lines: list[str], prefix: str, start: int = 0) -> int:
    for i in range(start, len(lines)):
        if lines[i].startswith(prefix):
            return i
    raise AssertionError(f"missing {prefix!r} from {start}")


class TestSecondaryTlbi(unittest.TestCase):
    def test_tlbi_after_hcr_and_before_sctlr_m(self) -> None:
        lines = global_asm(SECONDARY.read_text())
        hcr = find_line(lines, "msr hcr_el2")
        nxt = find_line(lines, "msr ", hcr + 1)
        window = lines[hcr + 1 : nxt]
        self.assertIn("tlbi vmalle1", window)
        self.assertLess(window.index("tlbi vmalle1"), window.index("dsb nsh"))

        ttbr = find_line(lines, "msr ttbr1_el1")
        sctlr = find_line(lines, "msr sctlr_el1", ttbr + 1)
        mid = lines[ttbr + 1 : sctlr]
        # Invalidate before the isb, then SCTLR_EL1. SCTLR_EL2.M is clear.
        self.assertLess(mid.index("tlbi vmalle1"), mid.index("isb"))
        self.assertLess(mid.index("tlbi vmalle1"), mid.index("dsb nsh"))
        self.assertLess(mid.index("dsb nsh"), mid.index("isb"))


class TestTtbr1Takeover(unittest.TestCase):
    def test_reserved_ttbr1_with_tlbi_between(self) -> None:
        text = MMU.read_text()
        lines = global_asm(text)
        first = find_line(lines, "msr ttbr1_el1")
        second = find_line(lines, "msr ttbr1_el1", first + 1)
        mid = lines[first + 1 : second]
        self.assertLess(mid.index("isb"), mid.index("tlbi vmalle1"))
        self.assertLess(mid.index("tlbi vmalle1"), mid.index("dsb nsh"))
        self.assertLess(mid.index("dsb nsh"), mid.index("isb", mid.index("dsb nsh")))

        fn = text.split("fn takeover_ttbr1", 1)[1]
        ttbr0 = fn.index("msr ttbr0_el1")
        blr = fn.index("blr", ttbr0)
        empty = fn.index("msr ttbr0_el1", blr)
        tlbi = fn.index("tlbi vmalle1", empty)
        dsb = fn.index("dsb nsh", tlbi)
        self.assertLess(ttbr0, blr)
        self.assertLess(blr, empty)
        self.assertLess(empty, tlbi)
        self.assertLess(tlbi, dsb)


if __name__ == "__main__":
    unittest.main()
