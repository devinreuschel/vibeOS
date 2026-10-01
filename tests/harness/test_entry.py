"""Host tests for scripts/check_entry.py (one entry path, AGENTS.md rule 1)."""

from __future__ import annotations

import contextlib
import io
import unittest
from unittest import mock

from scripts import check_entry
from scripts.check_entry import (
    nomem_toggle,
    scoped_files,
    stale_abi_feature,
    x86_interrupt_outside_arch,
)

# Built in pieces so a grep for the ABI string finds no fixture here.
ABI = 'extern "x86-' + 'interrupt"'
HANDLER = f"{ABI} fn h(_f: InterruptFrame) {{}}\n"
FEATURE = "#![no_std]\n#![feature(abi_x86_interrupt)]\n"


class TestOutsideArch(unittest.TestCase):
    def test_handler_in_src_is_reported(self) -> None:
        errs = x86_interrupt_outside_arch({"src/irq/irq_init.rs": "fn a() {}\n" + HANDLER})
        self.assertEqual(len(errs), 1)
        self.assertTrue(errs[0].startswith("src/irq/irq_init.rs:2: "), errs[0])

    def test_handler_under_crates_user_and_tests_is_reported(self) -> None:
        files = {"crates/x/src/y.rs": HANDLER, "user/bin/y.rs": HANDLER,
                 "tests/hostlib/src/z.rs": HANDLER}
        errs = x86_interrupt_outside_arch(files)
        self.assertEqual([e.split(":")[0] for e in errs], sorted(files))

    def test_handler_in_arch_is_allowed(self) -> None:
        self.assertEqual(x86_interrupt_outside_arch({"src/arch/x86_64/idt.rs": HANDLER}), [])

    def test_commented_line_is_ignored(self) -> None:
        files = {"src/console/kbd_init.rs": f"// was: {ABI} fn kbd()\n"
                                    f"fn kbd() {{}} // not {ABI}\n"}
        self.assertEqual(x86_interrupt_outside_arch(files), [])

    def test_spacing_variants_are_reported(self) -> None:
        spaced = ABI.replace(" ", "  ")
        errs = x86_interrupt_outside_arch({"src/a.rs": f"pub {spaced} fn h() {{}}\n"})
        self.assertEqual(len(errs), 1)

    def test_non_rust_file_is_ignored(self) -> None:
        self.assertEqual(x86_interrupt_outside_arch({"tests/harness/fixture.py": HANDLER}), [])


class TestStaleFeature(unittest.TestCase):
    def test_feature_with_no_handler_is_reported(self) -> None:
        errs = stale_abi_feature(FEATURE, {"src/main.rs": FEATURE, "src/a.rs": "fn a() {}\n"})
        self.assertEqual(len(errs), 1)
        self.assertTrue(errs[0].startswith("src/main.rs:2: "), errs[0])

    def test_feature_is_accepted_while_an_arch_handler_exists(self) -> None:
        files = {"src/main.rs": FEATURE, "src/arch/x86_64/idt.rs": HANDLER}
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


def _asm(template: str, options: str = "nomem, nostack") -> str:
    return f'fn f() {{\n    unsafe {{ asm!({template}, options({options})) }};\n}}\n'


class TestNomemToggle(unittest.TestCase):
    """`asm!` that toggles IF or AC is a compiler barrier (ROADMAP §10.3, F091)."""

    def flagged(self, text: str) -> list[str]:
        return nomem_toggle({"src/a.rs": text})

    def test_each_toggle_with_nomem_fails(self) -> None:
        for insn in ("cli", "sti", "stac", "clac"):
            with self.subTest(insn=insn):
                errs = self.flagged(_asm(f'"{insn}"'))
                self.assertEqual(len(errs), 1)
                self.assertTrue(errs[0].startswith("src/a.rs:2: "), errs[0])

    def test_multiline_core_arch_asm_fails(self) -> None:
        text = ("fn f() {\n    unsafe {\n        core::arch::asm!(\n"
                '            "pushfq",\n            "pop {0}",\n            "cli",\n'
                "            out(reg) _,\n            options(nomem),\n        );\n    }\n}\n")
        errs = self.flagged(text)
        self.assertEqual(len(errs), 1)
        self.assertTrue(errs[0].startswith("src/a.rs:3: "), errs[0])

    def test_cli_hlt_with_nomem_fails(self) -> None:
        self.assertEqual(len(self.flagged(_asm('"cli; hlt"'))), 1)

    def test_msr_daif_with_nomem_fails(self) -> None:
        self.assertEqual(len(self.flagged(_asm('"msr daifset, #2"'))), 1)
        self.assertEqual(len(self.flagged(_asm('"msr daifclr, #2"'))), 1)

    def test_sti_hlt_is_exempt(self) -> None:
        self.assertEqual(self.flagged(_asm('"sti; hlt"')), [])

    def test_sti_hlt_as_two_strings_is_exempt(self) -> None:
        self.assertEqual(self.flagged(_asm('"sti", "hlt"')), [])

    def test_sti_without_nomem_passes(self) -> None:
        self.assertEqual(self.flagged(_asm('"sti"', "nostack, preserves_flags")), [])

    def test_other_instruction_with_nomem_passes(self) -> None:
        self.assertEqual(self.flagged(_asm('"mov {}, cr2", out(reg) v')), [])

    def test_calls_and_comments_pass(self) -> None:
        text = ('fn f() {\n    x86::cli();\n    // asm!("cli", options(nomem))\n'
                '    /* asm!("sti", options(nomem)) */\n'
                '    let s = "asm!(\\"cli\\", options(nomem))";\n}\n')
        self.assertEqual(self.flagged(text), [])

    def test_global_asm_passes(self) -> None:
        self.assertEqual(self.flagged('global_asm!("cli", options(nomem));\n'), [])
        self.assertEqual(self.flagged('naked_asm!("cli", options(nomem));\n'), [])

    def test_word_containing_a_mnemonic_passes(self) -> None:
        self.assertEqual(self.flagged(_asm('"mov {0}, [rsp] // stack", out(reg) v')), [])
        self.assertEqual(self.flagged(_asm('"stack_top:", "nop"')), [])

    def test_char_literal_quote_does_not_open_a_string(self) -> None:
        text = "fn f() {\n    let q = '\"';\n" + _asm('"cli"').split("\n", 1)[1]
        self.assertEqual(len(self.flagged(text)), 1)

    def test_fixtures_from_8522be2(self) -> None:
        # Copied from the tree at 8522be2: each flagged line and each
        # `sti; hlt` that keeps `nomem`.
        flagged = {
            "src/x86.rs:146": '    unsafe { asm!("stac", options(nomem, nostack)) };\n',
            "src/x86.rs:156": '    unsafe { asm!("clac", options(nomem, nostack)) };\n',
            "src/x86.rs:267": '        unsafe { asm!("cli; hlt", options(nomem, nostack)) };\n',
            "src/x86.rs:303": '            unsafe { asm!("sti", options(nomem, nostack)) };\n',
            "src/x86.rs:493":
                '    unsafe { asm!("sti", options(nomem, nostack, preserves_flags)) };\n',
            "src/x86.rs:499":
                '    unsafe { asm!("cli", options(nomem, nostack, preserves_flags)) };\n',
            "src/console_init.rs:101":
                '            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));\n',
            "src/thread_init.rs:344":
                '            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));\n',
        }
        kept = {
            "src/console_init.rs:117":
                '            core::arch::asm!("sti; hlt", options(nomem, nostack));\n',
            "src/thread_init.rs:354":
                '            core::arch::asm!("sti; hlt", options(nomem, nostack));\n',
        }
        for where, line in flagged.items():
            with self.subTest(where=where):
                self.assertEqual(len(self.flagged(line)), 1)
        for where, line in kept.items():
            with self.subTest(where=where):
                self.assertEqual(self.flagged(line), [])

    def test_non_rust_file_is_ignored(self) -> None:
        self.assertEqual(nomem_toggle({"tests/harness/x.py": _asm('"cli"')}), [])


class TestTree(unittest.TestCase):
    def test_scope_is_rust_under_the_four_roots(self) -> None:
        files = scoped_files()
        self.assertIn("src/main.rs", files)
        self.assertIn("src/arch/x86_64/idt.rs", files)
        self.assertTrue(all(p.endswith(".rs") for p in files))
        self.assertTrue(all(p.split("/")[0] in check_entry.SCOPE for p in files))

    def test_clean_tree_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_entry.main([]), 0)
        self.assertEqual(out.getvalue().strip(), "check_entry: ok")

    def test_planted_handler_fails(self) -> None:
        planted = dict(scoped_files())
        planted["src/irq/irq_init.rs"] += HANDLER
        err = io.StringIO()
        with mock.patch.object(check_entry, "scoped_files", return_value=planted), \
                contextlib.redirect_stderr(err):
            self.assertEqual(check_entry.main([]), 1)
        self.assertIn("src/irq/irq_init.rs:", err.getvalue())

    def test_planted_nomem_toggle_fails(self) -> None:
        planted = dict(scoped_files())
        planted["src/arch/x86_64/cpu.rs"] += _asm('"cli"')
        err = io.StringIO()
        with mock.patch.object(check_entry, "scoped_files", return_value=planted), \
                contextlib.redirect_stderr(err):
            self.assertEqual(check_entry.main([]), 1)
        self.assertIn("src/arch/x86_64/cpu.rs:", err.getvalue())
        self.assertIn("nomem", err.getvalue())

    def test_usage(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(check_entry.main(["x"]), 2)


if __name__ == "__main__":
    unittest.main()
