"""Host tests for scripts/check_entry.py (one entry path, AGENTS.md rule 1)."""

from __future__ import annotations

import contextlib
import io
import unittest
from unittest import mock

from scripts import check_entry
from scripts.check_entry import scoped_files, stale_abi_feature, x86_interrupt_outside_arch

HANDLER = 'extern "x86-interrupt" fn h(_f: InterruptFrame) {}\n'
FEATURE = "#![no_std]\n#![feature(abi_x86_interrupt)]\n"


class TestOutsideArch(unittest.TestCase):
    def test_handler_in_src_is_reported(self) -> None:
        errs = x86_interrupt_outside_arch({"src/irq_init.rs": "fn a() {}\n" + HANDLER})
        self.assertEqual(len(errs), 1)
        self.assertTrue(errs[0].startswith("src/irq_init.rs:2: "), errs[0])

    def test_handler_under_crates_user_and_tests_is_reported(self) -> None:
        files = {"crates/x/src/lib.rs": HANDLER, "user/bin/y.rs": HANDLER,
                 "tests/hostlib/src/z.rs": HANDLER}
        errs = x86_interrupt_outside_arch(files)
        self.assertEqual([e.split(":")[0] for e in errs], sorted(files))

    def test_handler_in_arch_is_allowed(self) -> None:
        self.assertEqual(x86_interrupt_outside_arch({"src/arch/idt.rs": HANDLER}), [])

    def test_commented_line_is_ignored(self) -> None:
        files = {"src/kbd_init.rs": '// was: extern "x86-interrupt" fn kbd()\n'
                                    'fn kbd() {} // not extern "x86-interrupt"\n'}
        self.assertEqual(x86_interrupt_outside_arch(files), [])

    def test_spacing_variants_are_reported(self) -> None:
        errs = x86_interrupt_outside_arch({"src/a.rs": 'pub extern  "x86-interrupt" fn h() {}\n'})
        self.assertEqual(len(errs), 1)

    def test_non_rust_file_is_ignored(self) -> None:
        self.assertEqual(x86_interrupt_outside_arch({"tests/harness/fixture.py": HANDLER}), [])


class TestStaleFeature(unittest.TestCase):
    def test_feature_with_no_handler_is_reported(self) -> None:
        errs = stale_abi_feature(FEATURE, {"src/main.rs": FEATURE, "src/a.rs": "fn a() {}\n"})
        self.assertEqual(len(errs), 1)
        self.assertTrue(errs[0].startswith("src/main.rs:2: "), errs[0])

    def test_feature_is_accepted_while_an_arch_handler_exists(self) -> None:
        files = {"src/main.rs": FEATURE, "src/arch/idt.rs": HANDLER}
        self.assertEqual(stale_abi_feature(FEATURE, files), [])

    def test_commented_feature_is_ignored(self) -> None:
        text = "// #![feature(abi_x86_interrupt)]\n"
        self.assertEqual(stale_abi_feature(text, {"src/main.rs": text}), [])

    def test_combined_feature_list_is_reported(self) -> None:
        text = "#![feature(alloc_error_handler, abi_x86_interrupt)]\n"
        self.assertEqual(len(stale_abi_feature(text, {"src/main.rs": text})), 1)

    def test_clean_main_passes(self) -> None:
        text = "#![feature(alloc_error_handler)]\n"
        self.assertEqual(stale_abi_feature(text, {"src/main.rs": text}), [])


class TestTree(unittest.TestCase):
    def test_scope_is_rust_under_the_four_roots(self) -> None:
        files = scoped_files()
        self.assertIn("src/main.rs", files)
        self.assertIn("src/arch/idt.rs", files)
        self.assertTrue(all(p.endswith(".rs") for p in files))
        self.assertTrue(all(p.split("/")[0] in check_entry.SCOPE for p in files))

    def test_clean_tree_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_entry.main([]), 0)
        self.assertEqual(out.getvalue().strip(), "check_entry: ok")

    def test_planted_handler_fails(self) -> None:
        planted = dict(scoped_files())
        planted["src/irq_init.rs"] += HANDLER
        err = io.StringIO()
        with mock.patch.object(check_entry, "scoped_files", return_value=planted), \
                contextlib.redirect_stderr(err):
            self.assertEqual(check_entry.main([]), 1)
        self.assertIn("src/irq_init.rs:", err.getvalue())

    def test_usage(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(check_entry.main(["x"]), 2)


if __name__ == "__main__":
    unittest.main()
