"""Host tests for scripts/check_core_stable.py (vibeos-core on stable Rust)."""

from __future__ import annotations

import unittest

from scripts.check_core_stable import find_features


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


if __name__ == "__main__":
    unittest.main()
