"""Host tests for scripts/check_litmus.py (ROADMAP §11.7)."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts import check_litmus
from scripts.check_litmus import (
    check_helpers,
    check_sites,
    check_twins,
    litmus_files,
    twin_base,
    twin_diff_error,
    weaker_ops,
)


class Tree:
    def __init__(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        self.write(
            "src/arch/aarch64/mod.rs",
            "/// tests/litmus/seqlock.litmus\nfn dma_wmb() {}\n"
            "/// tests/litmus/log_ring.litmus\nfn dma_rmb() {}\n"
            "/// tests/litmus/dma_mb.litmus\nfn dma_mb() {}\n"
            "/// Arm ARM B2.3.5: Device vs Normal, herd7 does not model.\n"
            "unsafe fn mmio_read<T>() {}\n"
            "/// Arm ARM B2.3.5: Device vs Normal, herd7 does not model.\n"
            "unsafe fn mmio_write<T>() {}\n",
        )
        self.write("tests/litmus/seqlock.litmus", "AArch64 seqlock\n")
        self.write("tests/litmus/log_ring.litmus", "AArch64 log\n")
        self.write("tests/litmus/dma_mb.litmus", "AArch64 dma\n")

    def close(self) -> None:
        self._dir.cleanup()

    def write(self, rel: str, text: str) -> None:
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")


class LitmusTest(unittest.TestCase):
    def setUp(self) -> None:
        self.t = Tree()
        self.addCleanup(self.t.close)

    def test_named_tests_pass(self) -> None:
        self.assertEqual(check_helpers(self.t.root), [])

    def test_missing_comment_fails(self) -> None:
        self.t.write("src/arch/aarch64/mod.rs", "fn dma_wmb() {}\n")
        errs = check_helpers(self.t.root)
        self.assertTrue(any("dma_wmb" in e and "names no" in e for e in errs), errs)

    def test_missing_file_fails(self) -> None:
        self.t.write(
            "src/arch/aarch64/mod.rs",
            "/// tests/litmus/gone.litmus\nfn dma_wmb() {}\n",
        )
        errs = check_helpers(self.t.root)
        self.assertTrue(any("gone.litmus" in e for e in errs), errs)

    def test_arm_arm_citation_passes_without_file(self) -> None:
        self.t.write(
            "src/arch/aarch64/mod.rs",
            "/// Arm ARM G8.2: TLB maintenance, herd7 cannot express.\nfn dma_mb() {}\n",
        )
        self.assertEqual(check_helpers(self.t.root), [])

    def test_litmus_files_lists_tree(self) -> None:
        names = [p.name for p in litmus_files(self.t.root)]
        self.assertEqual(sorted(names), ["dma_mb.litmus", "log_ring.litmus", "seqlock.litmus"])


class HelperScanTest(unittest.TestCase):
    """Any port item with barrier instruction text is a helper."""

    def setUp(self) -> None:
        self.t = Tree()
        self.addCleanup(self.t.close)

    def scan(self, body: str, rel: str = "src/arch/aarch64/gic.rs") -> list[str]:
        self.t.write(rel, body)
        return [e for e in check_helpers(self.t.root) if rel in e]

    def test_asm_barrier_without_cite_fails(self) -> None:
        errs = self.scan('fn send() {\n    asm!("dsb ishst", "msr x, x0");\n}\n')
        self.assertTrue(any("send" in e and "names no" in e for e in errs), errs)

    def test_each_barrier_mnemonic_is_found(self) -> None:
        for ins in (
            "dmb ish",
            "dsb sy",
            "tlbi vmalle1",
            "dc cvac, {0}",
            "ic ivau, {0}",
            "    stlr x1, [x0]",
            "ldar x1, [x1]",
            "ldarb w1, [x1]",
            "ldaxr x1, [x0]",
        ):
            with self.subTest(ins=ins):
                errs = self.scan(f'fn f() {{\n    asm!("{ins}");\n}}\n')
                self.assertTrue(any("names no" in e for e in errs), errs)

    def test_plain_instructions_are_not_barriers(self) -> None:
        self.assertEqual(self.scan('fn f() {\n    asm!("isb", "msr daifset, #2");\n}\n'), [])
        self.assertEqual(self.scan('fn f() {\n    klog!("dcache line");\n}\n'), [])

    def test_doc_comment_cite_passes(self) -> None:
        body = (
            '/// Arm ARM DDI0487, "TLB maintenance requirements".\n'
            '#[inline]\nfn f() {\n    asm!("tlbi vmalle1");\n}\n'
        )
        self.assertEqual(self.scan(body), [])

    def test_comment_inside_the_item_passes(self) -> None:
        body = (
            'global_asm!(\n    "mov x0, x1",\n    // tests/litmus/seqlock.litmus\n'
            '    "stlr x0, [x1]",\n);\n'
        )
        self.assertEqual(self.scan(body), [])

    def test_comment_above_another_item_does_not_count(self) -> None:
        body = '/// Arm ARM B2.3.5\nfn other() {}\n\nfn f() {\n    asm!("dsb sy");\n}\n'
        errs = self.scan(body)
        self.assertTrue(any(":5: f `dsb sy` names no" in e for e in errs), errs)

    def test_a_citation_names_a_rule(self) -> None:
        def body(doc: str) -> str:
            return f'/// {doc}\nfn f() {{\n    asm!("dsb sy");\n}}\n'

        self.assertNotEqual(self.scan(body("Arm ARM DDI0487.")), [])
        self.assertNotEqual(self.scan(body("Instruction text (Arm ARM DDI0487).")), [])
        self.assertEqual(self.scan(body("Arm ARM B2.3.5: Device vs Normal.")), [])
        self.assertEqual(self.scan(body('Arm ARM DDI0487, "Memory barriers".')), [])
        wrapped = (
            '/// Arm ARM DDI0487, "TLB maintenance\n/// requirements".\n'
            'fn f() {\n    asm!("tlbi vmalle1");\n}\n'
        )
        self.assertEqual(self.scan(wrapped), [])

    def test_each_global_asm_barrier_needs_its_own_comment(self) -> None:
        filler = "".join(f'    "nop{n}",\n' for n in range(14))
        body = (
            'global_asm!(\n    // Arm ARM D8.13: TLB maintenance.\n    "tlbi vmalle1",\n'
            '    "dsb nsh",\n' + filler + '    "dsb ish",\n);\n'
        )
        errs = self.scan(body)
        self.assertEqual(len(errs), 1, errs)
        self.assertIn("global_asm `dsb ish`", errs[0])
        fixed = body.replace('    "dsb ish"', '    // tests/litmus/seqlock.litmus\n    "dsb ish"')
        self.assertEqual(self.scan(fixed), [])

    def test_comment_above_a_global_asm_does_not_count(self) -> None:
        body = '// Arm ARM D8.13: TLB maintenance.\nglobal_asm!(\n    "tlbi vmalle1",\n);\n'
        self.assertNotEqual(self.scan(body), [])

    def test_named_missing_file_fails(self) -> None:
        errs = self.scan('/// tests/litmus/gone.litmus\nfn f() {\n    asm!("dmb ish");\n}\n')
        self.assertTrue(any("gone.litmus" in e for e in errs), errs)

    def test_ktest_blocks_files_and_test_modules_are_skipped(self) -> None:
        body = 'global_asm!(\n    ".if {ktest}",\n    "ldarb w1, [x1]",\n    ".endif",\n);\n'
        self.assertEqual(self.scan(body), [])
        plain = 'fn f() {\n    asm!("dsb sy");\n}\n'
        self.assertEqual(self.scan(plain, "src/arch/aarch64/ktest_x.rs"), [])
        tests = (
            "#[cfg(test)]\nmod tests {\n    fn t() {\n"
            '        assert_eq!(X, "dsb ishst");\n    }\n}\n'
        )
        self.assertEqual(self.scan(tests, "crates/core/src/arch/aarch64/tlb.rs"), [])

    def test_barrier_after_a_ktest_block_is_checked(self) -> None:
        body = 'global_asm!(\n    ".if {ktest}",\n    ".endif",\n    "dsb sy",\n);\n'
        errs = self.scan(body)
        self.assertTrue(any("global_asm `dsb sy` names no" in e for e in errs), errs)


class SitesTest(unittest.TestCase):
    def setUp(self) -> None:
        self.t = Tree()
        self.addCleanup(self.t.close)

    def test_site_with_cite_passes(self) -> None:
        rel = "crates/core/src/mm/asid.rs"
        self.t.write(rel, "    /// tests/litmus/seqlock.litmus\n    pub fn switch_fast() {}\n")
        self.assertEqual(check_sites(self.t.root, ((rel, "switch_fast"),)), [])

    def test_site_without_cite_fails(self) -> None:
        rel = "crates/core/src/arch/aarch64/tlb.rs"
        self.t.write(rel, "/// Leaf change.\npub const LEAF_INVAL: [u8; 1] = [0];\n")
        errs = check_sites(self.t.root, ((rel, "LEAF_INVAL"),))
        self.assertTrue(any("LEAF_INVAL names no" in e for e in errs), errs)

    def test_gone_site_fails(self) -> None:
        self.t.write("src/arch/aarch64/cpu.rs", "fn other() {}\n")
        errs = check_sites(self.t.root, (("src/arch/aarch64/cpu.rs", "tlbi_va"),))
        self.assertTrue(any("tlbi_va, which is gone" in e for e in errs), errs)
        errs = check_sites(self.t.root, (("src/arch/aarch64/nope.rs", "tlbi_va"),))
        self.assertTrue(any("file that is gone" in e for e in errs), errs)

    def test_type_qualified_site_reads_that_impl(self) -> None:
        rel = "crates/core/src/irq/ipi.rs"
        self.t.write(
            rel,
            "impl Other {\n    pub fn push(&self) {}\n}\n\n"
            "impl<const W: usize> WakeInbox<W> {\n"
            "    /// tests/litmus/seqlock.litmus\n    pub fn push(&self) {}\n}\n",
        )
        self.assertEqual(check_sites(self.t.root, ((rel, "WakeInbox::push"),)), [])
        errs = check_sites(self.t.root, ((rel, "Other::push"),))
        self.assertTrue(any(":2: Other::push names no" in e for e in errs), errs)
        errs = check_sites(self.t.root, ((rel, "Other::drain"),))
        self.assertTrue(any("Other::drain, which is gone" in e for e in errs), errs)


MP_HEAD = "AArch64 mp\n(* comment *)\n{\n0:X1=flag; 0:X2=data;\n1:X1=flag; 1:X2=data;\n}\n"
MP_COND = "exists (1:X3=1 /\\ 1:X4=0)\n"


def mp(
    p0: tuple[str, ...] = ("MOV W0,#1", "STR W0,[X2]", "STLR W0,[X1]"),
    p1: tuple[str, ...] = ("LDAR W3,[X1]", "DMB ISHLD", "LDR W4,[X2]"),
    head: str = MP_HEAD,
    cond: str = MP_COND,
) -> str:
    """A message-passing test; the defaults are the fenced one."""
    n = max(len(p0), len(p1))
    rows = [" P0              | P1              ;"]
    for i in range(n):
        a = p0[i] if i < len(p0) else ""
        b = p1[i] if i < len(p1) else ""
        rows.append(f" {a:<16}| {b:<16};")
    return head + "\n".join(rows) + "\n" + cond


class TwinTest(unittest.TestCase):
    def setUp(self) -> None:
        self.t = Tree()
        self.addCleanup(self.t.close)
        for name in ("seqlock", "log_ring", "dma_mb"):
            (self.t.root / "tests" / "litmus" / f"{name}.litmus").unlink()

    def test_twin_base(self) -> None:
        self.assertEqual(twin_base("wake_inbox_relaxed.litmus"), "wake_inbox.litmus")
        self.assertEqual(twin_base("wake_inbox.relaxed.litmus"), "wake_inbox.litmus")
        self.assertEqual(twin_base("wake_inbox_no_release.litmus"), "wake_inbox.litmus")
        self.assertEqual(twin_base("seqlock_odd_llsc_relaxed.litmus"), "seqlock_odd_llsc.litmus")
        self.assertEqual(twin_base("dma_mb_x86_relaxed.litmus"), "dma_mb_x86.litmus")
        self.assertIsNone(twin_base("seqlock_odd.litmus"))
        self.assertIsNone(twin_base("wake_inbox_x86.litmus"))

    def test_weaker_ops(self) -> None:
        self.assertEqual(weaker_ops("CASAL"), {"CASA", "CASL", "CAS"})
        self.assertEqual(weaker_ops("casab"), {"CASB"})
        self.assertEqual(weaker_ops("LDSETL"), {"LDSET"})
        self.assertEqual(weaker_ops("STLRB"), {"STRB"})
        self.assertEqual(weaker_ops("LDAR"), {"LDR"})
        self.assertEqual(weaker_ops("LDAXR"), {"LDXR"})
        self.assertEqual(weaker_ops("LDSET"), set())
        self.assertEqual(weaker_ops("STR"), set())

    def test_test_without_twin_fails(self) -> None:
        self.t.write("tests/litmus/mp.litmus", mp())
        errs = check_twins(self.t.root)
        self.assertTrue(any("mp.litmus: no barrier-removed twin" in e for e in errs), errs)

    def test_x86_test_needs_no_twin(self) -> None:
        self.t.write(
            "tests/litmus/mp_x86.litmus", "X86 mp\n{\n}\n P0 ;\n MOV [x],$1 ;\nexists ([x]=1)\n"
        )
        self.assertEqual(check_twins(self.t.root), [])

    def test_twin_of_missing_test_fails(self) -> None:
        self.t.write("tests/litmus/mp_relaxed.litmus", mp())
        errs = check_twins(self.t.root)
        self.assertTrue(any("twin of missing mp.litmus" in e for e in errs), errs)

    def test_barrier_removed_twins_pass(self) -> None:
        self.t.write("tests/litmus/mp.litmus", mp())
        self.t.write(
            "tests/litmus/mp_no_release.litmus",
            mp(p0=("MOV W0,#1", "STR W0,[X2]", "STR W0,[X1]")),
        )
        self.t.write(
            "tests/litmus/mp_no_acquire.litmus",
            mp(p1=("LDR W3,[X1]", "DMB ISHLD", "LDR W4,[X2]")),
        )
        self.t.write("tests/litmus/mp_no_fence.litmus", mp(p1=("LDAR W3,[X1]", "LDR W4,[X2]")))
        self.t.write(
            "tests/litmus/mp_relaxed.litmus",
            mp(p0=("MOV W0,#1", "STR W0,[X2]", "STR W0,[X1]"), p1=("LDR W3,[X1]", "LDR W4,[X2]")),
        )
        self.assertEqual(check_twins(self.t.root), [])

    def test_twin_name_says_what_it_removes(self) -> None:
        no_rel = mp(p0=("MOV W0,#1", "STR W0,[X2]", "STR W0,[X1]"))
        no_acq = mp(p1=("LDR W3,[X1]", "DMB ISHLD", "LDR W4,[X2]"))
        no_ld_fence = mp(p1=("LDAR W3,[X1]", "LDR W4,[X2]"))
        ok = twin_diff_error
        self.assertIsNone(ok(mp(), no_rel, "mp_no_release.litmus"))
        self.assertIsNone(ok(mp(), no_acq, "mp_no_acquire.litmus"))
        self.assertIsNone(ok(mp(), no_ld_fence, "mp_no_acquire.litmus"))
        self.assertIsNone(ok(mp(), no_ld_fence, "mp_no_fence.litmus"))
        self.assertIsNone(ok(mp(), no_acq, "mp_no_ldar.litmus"))
        self.assertIn("not of its kind (release)", ok(mp(), no_acq, "mp_no_release.litmus") or "")
        self.assertIn(
            "not of its kind (release)", ok(mp(), no_ld_fence, "mp_no_release.litmus") or ""
        )
        self.assertIn("not of its kind (acquire)", ok(mp(), no_rel, "mp_no_acquire.litmus") or "")
        self.assertIn("not of its kind (fence)", ok(mp(), no_acq, "mp_no_fence.litmus") or "")
        casal = mp(p0=("MOV W0,#1", "STR W0,[X2]", "CASAL W4,W0,[X1]"))
        self.assertIsNone(
            ok(casal, mp(p0=("MOV W0,#1", "STR W0,[X2]", "CASA W4,W0,[X1]")), "x_no_release")
        )
        self.assertIsNotNone(
            ok(casal, mp(p0=("MOV W0,#1", "STR W0,[X2]", "CASL W4,W0,[X1]")), "x_no_release")
        )
        self.t.write("tests/litmus/mp.litmus", mp())
        self.t.write("tests/litmus/mp_no_release.litmus", no_acq)
        errs = check_twins(self.t.root)
        self.assertTrue(any("mp_no_release.litmus: a _no_release twin" in e for e in errs), errs)

    def test_twin_that_keeps_every_barrier_fails(self) -> None:
        self.assertIn("keeps every barrier", twin_diff_error(mp(), mp()) or "")

    def test_twin_that_changes_more_than_a_barrier_fails(self) -> None:
        cases = {
            "other operand": mp(p0=("MOV W0,#1", "STR W0,[X2]", "STR W0,[X2]")),
            "other instruction": mp(p0=("MOV W0,#2", "STR W0,[X2]", "STR W0,[X1]")),
            "condition": mp(
                p1=("LDR W3,[X1]", "DMB ISHLD", "LDR W4,[X2]"), cond="exists (1:X3=1)\n"
            ),
            "init": mp(
                p1=("LDR W3,[X1]", "DMB ISHLD", "LDR W4,[X2]"),
                head=MP_HEAD.replace("0:X1=flag;", "0:X1=data;"),
            ),
            "extra": mp(p1=("LDR W3,[X1]", "DMB ISHLD", "LDR W4,[X2]", "DMB ISH")),
            "moved": mp(p1=("DMB ISHLD", "LDR W3,[X1]", "LDR W4,[X2]")),
            "strengthened": mp(p0=("MOV W0,#1", "STLR W0,[X2]", "STR W0,[X1]")),
            "arch": mp(
                p1=("LDR W3,[X1]", "DMB ISHLD", "LDR W4,[X2]"),
                head=MP_HEAD.replace("AArch64", "X86"),
            ),
        }
        for what, twin in cases.items():
            with self.subTest(what=what):
                self.assertIsNotNone(twin_diff_error(mp(), twin))
        self.t.write("tests/litmus/mp.litmus", mp())
        self.t.write("tests/litmus/mp_relaxed.litmus", cases["other operand"])
        errs = check_twins(self.t.root)
        self.assertTrue(any("mp_relaxed.litmus: P0" in e for e in errs), errs)


class TestRepo(unittest.TestCase):
    def test_repo_helpers_pass(self) -> None:
        self.assertEqual(check_helpers(), [])

    def test_repo_sites_pass(self) -> None:
        self.assertEqual(check_sites(), [])

    def test_repo_twins_pass(self) -> None:
        self.assertEqual(check_twins(), [])

    def test_wake_inbox_has_one_twin_per_barrier(self) -> None:
        names = {p.name for p in litmus_files(check_litmus.ROOT)}
        self.assertIn("wake_inbox_no_release.litmus", names)
        self.assertIn("wake_inbox_no_acquire.litmus", names)
        self.assertNotIn("wake_inbox_relaxed.litmus", names)

    def test_usage(self) -> None:
        self.assertEqual(check_litmus.main(["--nope"]), 2)


class ObservationTest(unittest.TestCase):
    def test_both_models_use_the_same_rule(self) -> None:
        err = check_litmus.observation_error
        self.assertIsNone(err("tests/litmus/seqlock.litmus", "aarch64.cat", "Never"))
        self.assertIsNotNone(err("tests/litmus/seqlock.litmus", "aarch64.cat", "Sometimes"))
        self.assertIsNone(err("tests/litmus/seqlock_x86.litmus", "x86tso.cat", "Never"))
        self.assertIsNotNone(err("tests/litmus/seqlock_x86.litmus", "x86tso.cat", "Sometimes"))
        self.assertIsNone(err("tests/litmus/dma_mb_relaxed.litmus", "aarch64.cat", "Sometimes"))
        self.assertIsNone(err("tests/litmus/dma_mb_x86_relaxed.litmus", "x86tso.cat", "Always"))
        self.assertIsNotNone(err("tests/litmus/dma_mb_x86_relaxed.litmus", "x86tso.cat", "Never"))
        self.assertIsNone(
            err("tests/litmus/wake_inbox_no_release.litmus", "aarch64.cat", "Sometimes")
        )
        self.assertIsNotNone(
            err("tests/litmus/wake_inbox_no_acquire.litmus", "aarch64.cat", "Never")
        )

    def test_run_herd7_reads_x86_verdicts(self) -> None:
        t = Tree()
        self.addCleanup(t.close)
        t.write("tests/litmus/mp_x86.litmus", "X86 mp\n")

        def fake_run(_cmd: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(_cmd, 0, "Observation mp Sometimes 1 0\n", "")

        with (
            patch("scripts.check_litmus.which", return_value="/usr/bin/herd7"),
            patch("scripts.check_litmus.subprocess.run", side_effect=fake_run),
        ):
            errs = check_litmus.run_herd7(t.root)
        self.assertTrue(any("mp_x86.litmus" in e and "x86tso.cat" in e for e in errs), errs)


if __name__ == "__main__":
    unittest.main()
