"""Host tests for scripts/check_core_stable.py (vibeos-core on stable Rust)."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts import check_core_stable
from scripts.check_core_stable import find_features, missing_parser_attrs


class TestFindFeatures(unittest.TestCase):
    def test_plain_feature_attr(self) -> None:
        self.assertEqual(find_features("//! doc\n#![feature(allocator_api)]\n"), [2])

    def test_cfg_attr_feature(self) -> None:
        text = "#![cfg_attr(not(test), feature(core_intrinsics))]\n"
        self.assertEqual(find_features(text), [1])

    def test_cargo_feature_predicate_is_not_a_language_feature(self) -> None:
        text = '#![cfg_attr(not(any(test, feature = "std")), no_std)]\n#![deny(clippy::panic)]\n'
        self.assertEqual(find_features(text), [])

    def test_item_level_and_comment_lines_ignored(self) -> None:
        text = "// #![feature(x)] in a comment\n#[cfg(feature = \"std\")]\npub mod a;\n"
        self.assertEqual(find_features(text), [])


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


if __name__ == "__main__":
    unittest.main()
