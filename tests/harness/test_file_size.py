"""Host tests for scripts/check_file_size.py (ROADMAP §10.3, Q5)."""

from __future__ import annotations

import unittest

from scripts.check_file_size import (
    LIMIT,
    ROOT,
    SPLIT_BY,
    check,
    count_lines,
    is_source,
)
from scripts.gatelib import Box, match_key, roadmap_boxes

OPEN = Box(10, False, "`a.rs` split by responsibility (Q5)", "10.3", 10)
TICKED = Box(11, True, "`b.rs` split by responsibility (Q5)", "10.3", 10)
OTHER = Box(12, False, "`c.rs` split by responsibility (Q5) again", "10.3", 10)
BOXES = [OPEN, TICKED, OTHER]


class TestCountLines(unittest.TestCase):
    def test_counts(self) -> None:
        self.assertEqual(count_lines(""), 0)
        self.assertEqual(count_lines("a\n"), 1)
        self.assertEqual(count_lines("a\nb"), 2)


class TestIsSource(unittest.TestCase):
    def test_suffixes(self) -> None:
        for p in ("src/a.rs", "src/arch/x86_64/trampoline.S", "lib/start.asm", "lib/defs.inc"):
            self.assertTrue(is_source(p), p)
        for p in ("docs/ROADMAP.md", "Cargo.toml", "scripts/check_file_size.py"):
            self.assertFalse(is_source(p), p)


class TestCheck(unittest.TestCase):
    def test_at_the_limit_passes(self) -> None:
        self.assertEqual(check({"src/a.rs": LIMIT}, [], BOXES), [])

    def test_unlisted_file_over_the_limit_fails(self) -> None:
        errs = check({"src/a.rs": LIMIT + 1, "src/b.rs": 3}, [], BOXES)
        self.assertEqual(len(errs), 1)
        self.assertIn("src/a.rs", errs[0])
        self.assertIn(str(LIMIT + 1), errs[0])

    def test_listed_with_an_open_box_passes(self) -> None:
        self.assertEqual(check({"src/a.rs": LIMIT + 1}, [("src/a.rs", "`a.rs` split")], BOXES), [])

    def test_listed_with_a_ticked_box_fails(self) -> None:
        errs = check({"src/b.rs": LIMIT + 1}, [("src/b.rs", "`b.rs` split")], BOXES)
        self.assertEqual(len(errs), 1)
        self.assertIn("ticked", errs[0])

    def test_key_matching_no_box_fails(self) -> None:
        errs = check({"src/a.rs": LIMIT + 1}, [("src/a.rs", "no such box")], BOXES)
        self.assertEqual(len(errs), 1)
        self.assertIn("matches no line", errs[0])

    def test_key_matching_two_boxes_fails(self) -> None:
        errs = check({"src/a.rs": LIMIT + 1}, [("src/a.rs", "split by responsibility")], BOXES)
        self.assertEqual(len(errs), 1)
        self.assertIn("matches 3 lines", errs[0])

    def test_stale_row_for_a_missing_file_fails(self) -> None:
        errs = check({}, [("src/a.rs", "`a.rs` split")], BOXES)
        self.assertEqual(len(errs), 1)
        self.assertIn("gone", errs[0])

    def test_stale_row_for_a_short_file_fails(self) -> None:
        errs = check({"src/a.rs": LIMIT}, [("src/a.rs", "`a.rs` split")], BOXES)
        self.assertEqual(len(errs), 1)
        self.assertIn("delete the row", errs[0])

    def test_duplicate_row_fails(self) -> None:
        rows = [("src/a.rs", "`a.rs` split"), ("src/a.rs", "`a.rs` split")]
        errs = check({"src/a.rs": LIMIT + 1}, rows, BOXES)
        self.assertEqual(len(errs), 1)
        self.assertIn("twice", errs[0])


class TestRealTree(unittest.TestCase):
    def test_each_key_names_one_open_box(self) -> None:
        roadmap = ROOT / "docs" / "ROADMAP.md"
        boxes = roadmap_boxes(roadmap)
        lines = roadmap.read_text(encoding="utf-8").splitlines()
        for path, key in SPLIT_BY:
            with self.subTest(path=path):
                self.assertFalse(match_key(key, boxes, lines).ticked)


if __name__ == "__main__":
    unittest.main()
