"""aarch64 MMU bring-up sequences (#260).

The secondary stub invalidates the local TLB after the HCR_EL2 E2H/TGE
write and again after the TTBR writes, before the isb that publishes
SCTLR.M. The boot CPU installs TTBR1 through a reserved empty root with
a local TLBI between the two writes, from a TTBR0 identity map.

ROADMAP §11.4: the stub writes no memory and runs no exclusive or atomic
until its MMU and caches are on, and its first store is the arrival
`stlr`; the boot CPU cleans what the stub reads to PoC before `CPU_ON`.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SECONDARY = ROOT / "src/arch/aarch64/secondary.rs"
MMU = ROOT / "src/arch/aarch64/mmu.rs"
SMP_INIT = ROOT / "src/smp/smp_init.rs"


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


# A64 stores and atomics: every `st*` (plain, pair, release, exclusive, and
# the LSE `st<op>` aliases), the LSE `ld<op>`, `cas`, and `swp` families, the
# exclusive loads, which open a monitor, and `dc zva`, which writes a block.
STORE_OR_ATOMIC = re.compile(
    r"^(st[a-z0-9]*"
    r"|ld(add|clr|eor|set|smax|smin|umax|umin)[a-z]*"
    r"|lda?x[rp][bh]?"
    r"|cas[a-z]*"
    r"|swp[a-z]*"
    r"|dc\s+zva)\b"
)


def fn_body(text: str, name: str) -> str:
    """The text of `fn name` up to the next top-level item."""
    start = text.index(f"fn {name}(")
    # Items start at column 0; a fn's own lines are indented.
    end = re.search(r"\n[#a-z]", text[start + 1 :])
    return text[start : start + 1 + end.start()] if end else text[start:]


class TestSecondaryNoSharedWrite(unittest.TestCase):
    """ROADMAP §11.4: before `msr sctlr_el1` (MMU and caches on) the stub
    writes no memory and runs no exclusive or atomic; its first store is
    the arrival `stlr` to the status word, after the MMU enable."""

    def test_store_pattern_matches_the_forms(self) -> None:
        for insn in ("str x1, [x0]", "stp x1, x2, [sp]", "stlr x19, [x6]",
                     "stxr w2, x1, [x0]", "stlxr w2, x1, [x0]", "ldxr x1, [x0]",
                     "ldaxr x1, [x0]", "ldadd x1, x2, [x0]", "ldsetal x1, x2, [x0]",
                     "casal x1, x2, [x0]", "swp x1, x2, [x0]", "stadd x1, [x0]",
                     "dc zva, x0", "sturb w1, [x0]"):
            self.assertTrue(STORE_OR_ATOMIC.match(insn), insn)
        for insn in ("ldr x1, [x0, #8]", "ldp x1, x2, [x0]", "ldar x1, [x0]",
                     "ldur x1, [x0]", "dc cvac, x0", "msr sctlr_el1, x1"):
            self.assertFalse(STORE_OR_ATOMIC.match(insn), insn)

    def test_nothing_before_sctlr_then_arrival_stlr(self) -> None:
        lines = global_asm(SECONDARY.read_text())
        sctlr = find_line(lines, "msr sctlr_el1")
        early = [ln for ln in lines[:sctlr] if STORE_OR_ATOMIC.match(ln)]
        self.assertEqual(early, [], "store or atomic before the MMU enable")
        stores = [i for i, ln in enumerate(lines) if STORE_OR_ATOMIC.match(ln)]
        self.assertTrue(stores, "the stub publishes no arrival")
        first = lines[stores[0]]
        self.assertGreater(stores[0], sctlr)
        self.assertTrue(first.startswith("stlr "), first)
        # Its address is the param page's status word, and the conduit load
        # comes before it, so the store is the stub's last param-page access.
        base = first.split("[", 1)[1].rstrip("]").strip()
        addr = max(i for i in range(stores[0]) if lines[i].startswith(f"add {base},"))
        self.assertIn("#{status}", lines[addr])
        hvc = find_line(lines, "ldr x1, [x5, #{hvc}]")
        self.assertLess(hvc, stores[0])
        after = [ln for ln in lines[stores[0] + 1 :] if ln.startswith(("ldr", "ldp"))]
        self.assertEqual(after, [], "param-page load after the arrival store")


class TestCleanBeforeCpuOn(unittest.TestCase):
    """ROADMAP §11.4: the boot CPU cleans the stub, the identity tables, and
    the param page to PoC (`dc cvac`, then `dsb sy`) before `CPU_ON`."""

    def test_clean_poc_is_dc_cvac_then_dsb_sy(self) -> None:
        body = fn_body(SECONDARY.read_text(), "clean_poc")
        self.assertLess(body.index('"dc cvac'), body.index('"dsb sy"'))

    def test_stub_and_tables_cleaned_before_bring_up(self) -> None:
        body = fn_body(SMP_INIT.read_text(), "init_aarch64")
        tables = body.index("clean_identity(&id_tables")
        stub = body.index("clean_poc(stub_va, stub_len)")
        start = body.index("start_one_aarch64(")
        self.assertLess(tables, start)
        self.assertLess(stub, start)

    def test_param_and_new_tables_cleaned_before_cpu_on(self) -> None:
        body = fn_body(SMP_INIT.read_text(), "start_one_aarch64")
        cpu_on = body.index("power::cpu_on(")
        mapped = body.index("map_identity(ttbr0_id, param_pa")
        tables = body.index("clean_identity(id_tables", mapped)
        write = body.index("write_volatile(param)")
        param = body.index("clean_poc(\n        param_va", write)
        self.assertLess(tables, cpu_on)
        self.assertLess(write, param)
        self.assertLess(param, cpu_on)


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
