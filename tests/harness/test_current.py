"""Host tests for scripts/check_current.py (ROADMAP §10.3, F039: `current` is
read in one instruction that preemption cannot split)."""

from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import check_current

SRC = "src/sched/foo_init.rs"
USES = "use crate::per_cpu_init;\n"


class TestFindCurrentReads(unittest.TestCase):
    def test_reads_outside_arch_are_flagged(self) -> None:
        cases = {
            "field read": USES + "fn f() { let t = cpu.current; }\n",
            "comparison": USES + "fn f() -> bool { t == cpu.current }\n",
            "equality left": USES + "fn f() -> bool { cpu.current == cpu.idle }\n",
            "offset_of": "const O: usize = offset_of!(PerCpu, current);\n",
            "macro": "fn f() { let t = crate::per_cpu!(current); }\n",
        }
        for what, text in cases.items():
            with self.subTest(what=what):
                self.assertEqual(check_current.find_current_reads(SRC, text), [text.count("\n")])

    def test_crates_are_in_scope(self) -> None:
        text = "use crate::per_cpu::PerCpu;\nfn f(c: &PerCpu) -> bool { c.current.is_null() }\n"
        self.assertEqual(check_current.find_current_reads("crates/core/src/x.rs", text), [2])

    def test_allowed_forms_pass(self) -> None:
        cases = {
            "under src/arch": ("src/arch/x86_64/percpu.rs",
                               "const O: usize = offset_of!(PerCpu, current);\n"
                               "fn f(c: &PerCpu) { let t = c.current; }\n"),
            "the PerCpu file": ("crates/core/src/smp/per_cpu.rs",
                                "pub struct PerCpu { pub current: u64 }\n"
                                "fn t(p: PerCpu) { assert!(p.current == 0); }\n"),
            "a write": (SRC, USES + "fn f(cpu: &mut PerCpu, t: u64) { cpu.current = t; }\n"),
            "a call": (SRC, USES + "fn f() { let c = per_cpu_init::current(); }\n"),
            "a longer name": (SRC, USES + "fn f() { let c = s.current_id; }\n"),
            "a line comment": (SRC, USES + "// cpu.current and per_cpu!(current)\n"),
            "a block comment": (SRC, USES + "/* offset_of!(PerCpu, current)\n cpu.current */\n"),
            "no PerCpu name": (SRC, "fn f(w: &Walk) -> u32 { w.current }\n"),
        }
        for what, (path, text) in cases.items():
            with self.subTest(what=what):
                self.assertEqual(check_current.find_current_reads(path, text), [])

    def test_line_numbers_survive_comments(self) -> None:
        text = USES + "/* one\ntwo */\nfn f() { let t = cpu.current; }\n"
        self.assertEqual(check_current.find_current_reads(SRC, text), [4])


class TestMain(unittest.TestCase):
    def test_main_over_a_tree(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            bad = root / SRC
            bad.parent.mkdir(parents=True)
            bad.write_text(USES + "fn f() { let t = cpu.current; }\n")
            arch = root / "src/arch/x86_64/percpu.rs"
            arch.parent.mkdir(parents=True)
            arch.write_text("fn f(c: &PerCpu) { let t = c.current; }\n")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(check_current.main(["--root", d]), 1)
            self.assertIn(f"{SRC}:2: reads PerCpu.current outside src/arch/", out.getvalue())
            bad.write_text(USES + "fn f() { let t = crate::arch::current_tcb(); }\n")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(check_current.main(["--root", d]), 0)
            self.assertIn("check_current: ok (2 files)", out.getvalue())

    def test_this_repository_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_current.main([]), 0)


if __name__ == "__main__":
    unittest.main()
