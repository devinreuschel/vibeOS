"""Host tests for scripts/check_user_arch.py (ROADMAP §10.5, the one `arch` module box)."""

from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import check_user_arch

REPO = Path(__file__).resolve().parents[2]

CLEAN = {
    "user/Cargo.toml": '[package]\nname = "vibeos-user"\n',
    "user/mem/Cargo.toml": '[package]\nname = "vibeos-user-mem"\n',
    "user/mem/src/lib.rs": "#![no_std]\n#![no_builtins]\n",
    "user/src/lib.rs": "#![no_std]\nmod arch;\npub mod rt;\n",
    "user/src/rt.rs": "// SAFETY: established by rt::start.\npub fn f() {}\n",
    "user/src/bin/p.rs": "#![no_std]\n#![no_main]\n",
}


class TestCheckUserArch(unittest.TestCase):
    def tree(self, extra: dict[str, str]) -> tuple[list[str], int]:
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            for rel, text in {**CLEAN, **extra}.items():
                p = root / rel
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(text)
            return check_user_arch.check(root)

    def test_clean_tree_passes(self) -> None:
        errs, n = self.tree({})
        self.assertEqual(errs, [])
        self.assertEqual(n, len(CLEAN))

    def test_arch_names_in_portable_files_fail(self) -> None:
        cases = {
            "user/src/lib.rs": '#[cfg(target_arch = "x")]\nmod a;\n',
            "user/src/rt.rs": "fn f() { unsafe { core::arch::asm!(\"\") } }\n",
            "user/src/bin/p.rs": "// Only runs on x86_64 today.\n",
            "user/mem/src/lib.rs": "//! Portable, even to aarch64.\n",
        }
        for rel, text in cases.items():
            with self.subTest(rel=rel):
                errs, _ = self.tree({rel: text})
                self.assertEqual(len(errs), 1, errs)
                self.assertTrue(errs[0].startswith(f"{rel}:"), errs)
                self.assertIn("names an architecture outside user/src/arch/", errs[0])

    def test_other_rules_fail(self) -> None:
        for text in ("#[unsafe(naked)]\n", "naked_asm!(\"\")\n", "global_asm!(\"\")\n",
                     "use std::arch::x;\n", "let v = rdi;\n", "// clobbers r11\n",
                     "// amd64 and riscv64\n"):
            with self.subTest(text=text):
                errs, _ = self.tree({"user/src/io.rs": text})
                self.assertEqual(len(errs), 1, errs)

    def test_arch_directory_passes(self) -> None:
        text = ('#[cfg(target_arch = "x86_64")]\n'
                "core::arch::asm!(\"syscall\", in(\"rax\") n, out(\"rcx\") _);\n")
        errs, _ = self.tree({"user/src/arch/x86_64/mod.rs": text,
                             "user/src/arch/mod.rs": text})
        self.assertEqual(errs, [])

    def test_match_arm_passes(self) -> None:
        errs, _ = self.tree({"user/src/env.rs": "// The last match arm.\nlet arm = 1;\n"
                                                 "let x0 = 0;\n"})
        self.assertEqual(errs, [])

    def test_assembly_programs_ignored(self) -> None:
        errs, n = self.tree({"user/tests.asm": "mov rax, 60\nsyscall\n",
                             "user/sys.inc": "; x86_64\n"})
        self.assertEqual(errs, [])
        self.assertEqual(n, len(CLEAN))

    def test_real_tree_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_user_arch.main(["--root", str(REPO)]), 0)
        self.assertIn("check_user_arch: ok (", out.getvalue())


if __name__ == "__main__":
    unittest.main()
