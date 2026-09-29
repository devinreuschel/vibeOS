"""Host tests for scripts/check_core_stable.py (vibeos-core's byte-parser lints, and its ban on
assembly and `cfg(target_arch)`)."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts import check_core_stable
from scripts.check_core_stable import (
    arch_code_errors,
    core_files,
    find_arch_code,
    missing_parser_attrs,
)

DENY = "#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]\n"
OUTER = ("#[deny(\n    clippy::indexing_slicing,\n    clippy::arithmetic_side_effects\n)]\n")
ROWS = (
    ("a/mod.rs", "inner", ""),
    ("b.rs", "fn", "parse"),
    ("c.rs", "fn", "Decoder::feed"),
    ("d/mod.rs", "allow", "§14.8"),
)
GOOD = {
    "a/mod.rs": "//! A parser.\n" + DENY + "pub fn x() {}\n",
    "b.rs": "/// Parse.\n" + OUTER + "pub fn parse(b: &[u8]) -> u8 {\n    b[0]\n}\n",
    "c.rs": ("pub struct Decoder;\nimpl Decoder {\n" + "".join("    " + ln + "\n" for ln in
             OUTER.splitlines()) + "    pub fn feed(&mut self) {}\n}\n"
             "#[cfg(test)]\nmod tests {\n    fn feed() {}\n}\n"),
    "d/mod.rs": ("#![allow(\n    clippy::indexing_slicing,\n    clippy::arithmetic_side_effects,\n"
                 "    reason = \"vibefs v1, retired by ROADMAP §14.8\"\n)]\n"),
}
BARE = {
    "a/mod.rs": "pub fn x() {}\n",
    "b.rs": "pub fn parse(b: &[u8]) -> u8 {\n    b[0]\n}\n",
    "c.rs": "pub struct Decoder;\nimpl Decoder {\n    pub fn feed(&mut self) {}\n}\n"
            "#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]\nfn feed() {}\n",
    "d/mod.rs": "#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]\n",
}
KEYS = ("a/mod.rs", "b.rs::parse", "c.rs::Decoder::feed", "d/mod.rs")


def check(files: dict[str, str], pending: tuple[str, ...] = ()) -> list[str]:
    with tempfile.TemporaryDirectory() as t:
        root = Path(t)
        for rel, text in files.items():
            (root / rel).parent.mkdir(parents=True, exist_ok=True)
            (root / rel).write_text(text)
        return missing_parser_attrs(root, ROWS, pending)


class TestParserAttrs(unittest.TestCase):
    def test_present(self) -> None:
        self.assertEqual(check(GOOD), [])

    def test_missing(self) -> None:
        errors = check(BARE)
        self.assertEqual(len(errors), 4, errors)
        for key, e in zip(KEYS, errors, strict=True):
            self.assertTrue(e.startswith(f"{key}: needs"), e)

    def test_pending(self) -> None:
        self.assertEqual(check(BARE, KEYS), [])

    def test_stale_pending(self) -> None:
        errors = check(GOOD, KEYS)
        self.assertEqual(len(errors), 4, errors)
        self.assertIn("remove the entry", errors[0])

    def test_function(self) -> None:
        one_lint = {**GOOD, "b.rs": "#[deny(clippy::indexing_slicing)]\npub fn parse() {}\n"}
        self.assertEqual(len(check(one_lint)), 1)
        gone = {**GOOD, "b.rs": "pub fn other() {}\n"}
        self.assertIn("fn `parse` is missing", check(gone)[0])

    def test_method(self) -> None:
        # The deny on the test module's free `feed` is not the method's.
        errors = check({**GOOD, "c.rs": BARE["c.rs"]})
        self.assertEqual(len(errors), 1)
        self.assertTrue(errors[0].startswith("c.rs::Decoder::feed: needs"), errors)

    def test_missing_file(self) -> None:
        files = dict(GOOD)
        del files["a/mod.rs"]
        self.assertEqual(check(files, KEYS[:1]), ["a/mod.rs: listed parser file is missing"])

    def test_real_tree(self) -> None:
        self.assertEqual(missing_parser_attrs(check_core_stable.CORE_ROOT.parent), [])
        keys = {f"{r}::{t}" if k == "fn" else r for r, k, t in check_core_stable.PARSERS}
        self.assertLessEqual(set(check_core_stable.PARSERS_PENDING), keys)


class TestArchCode(unittest.TestCase):
    def test_asm_macros_found(self) -> None:
        text = ("fn a() {\n    unsafe { asm!(\"nop\") };\n}\n"
                "fn b() {\n    unsafe { core::arch::asm!(\"nop\") };\n}\n"
                "global_asm!(\"\");\n"
                "#[unsafe(naked)]\nextern \"C\" fn c() {\n    naked_asm!(\"ret\")\n}\n"
                "my_asm!(x);\nmy_global_asm!(x);\n")
        self.assertEqual(find_arch_code(text), [2, 5, 7, 10])

    def test_target_arch_in_every_cfg_form(self) -> None:
        text = ('#[cfg(target_arch = "x86_64")]\nfn a() {}\n'
                '#[cfg(all(test, target_arch = "x86_64"))]\nfn b() {}\n'
                '#[cfg_attr(target_arch = "x86_64", inline)]\nfn c() {}\n'
                'fn d() -> bool {\n    cfg!(target_arch = "aarch64")\n}\n')
        self.assertEqual(find_arch_code(text), [1, 3, 5, 8])

    def test_test_module_not_exempt(self) -> None:
        text = ('pub fn x() {}\n#[cfg(test)]\nmod tests {\n'
                '    #[cfg(target_arch = "x86_64")]\n    #[test]\n    fn t() {\n'
                '        unsafe { core::arch::asm!("nop") };\n    }\n}\n')
        self.assertEqual(find_arch_code(text), [4, 7])

    def test_comments_and_target_os_not_flagged(self) -> None:
        text = ('//! The switch asm! lives in the port; no target_arch here.\n'
                '/// Not `global_asm!` either.\n'
                '// cfg(target_arch = "x86_64")\n'
                '/* asm!("nop")\n   /* nested target_arch */ still a comment */\n'
                '#[cfg(target_os = "none")]\nfn a() {}\n'
                'const URL: &str = "http://x"; // asm!\n'
                "const Q: char = '\"'; // target_arch\n"
                'const R: &str = r#"say "hi" // "#; // asm!\n')
        self.assertEqual(find_arch_code(text), [])

    def test_core_files_follows_path_attribute(self) -> None:
        with tempfile.TemporaryDirectory() as t:
            root = Path(t)
            src = root / "core" / "src"
            (src / "dev").mkdir(parents=True)
            (root / "outside").mkdir()
            (src / "lib.rs").write_text('#[path = "../outside/x.rs"]\nmod x;\npub mod dev;\n')
            (src / "dev" / "mod.rs").write_text("pub fn f() {}\n")
            # Only a real attribute counts, not one in a comment.
            (root / "core" / "outside").mkdir()
            (root / "core" / "outside" / "x.rs").write_text(
                '// #[path = "../../ignored.rs"]\n#[cfg(target_arch = "x86_64")]\npub fn g() {}\n')
            (root / "ignored.rs").write_text("global_asm!(\"\");\n")
            files = core_files(src)
            self.assertEqual(files, sorted([(src / "lib.rs").resolve(),
                                            (src / "dev" / "mod.rs").resolve(),
                                            (root / "core" / "outside" / "x.rs").resolve()]))
            errors = arch_code_errors(src)
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("x.rs:2:", errors[0])

    def test_repo_core_is_clean(self) -> None:
        src = check_core_stable.CORE_ROOT.parent
        cell = (check_core_stable.ROOT / "src" / "cell.rs").resolve()
        self.assertIn(cell, core_files(src))
        self.assertEqual(arch_code_errors(src), [])


if __name__ == "__main__":
    unittest.main()
