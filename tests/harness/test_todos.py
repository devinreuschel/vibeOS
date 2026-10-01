"""Host tests for scripts/check_todos.py. Every marker word is built from parts, so this
file passes its own check."""

from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import check_todos
from scripts.check_todos import check_text
from scripts.check_todos import check_todos as check

WORDS = ("TO" + "DO", "FIX" + "ME", "X" + "XX")
CITE = "ROADMAP §" + "10.9"


class TestTodos(unittest.TestCase):
    def test_each_word_fails(self) -> None:
        for w in WORDS:
            with self.subTest(word=w):
                got = check_text("src/a.rs", f"fn f() {{}}\n// {w}: later\n")
                self.assertEqual(got, [f"src/a.rs:2: {w} without a `ROADMAP §N.M` on its line"])

    def test_cited_on_same_line_passes(self) -> None:
        for w in WORDS:
            with self.subTest(word=w):
                self.assertEqual(check_text("src/a.rs", f"// {w}: the gap, {CITE}\n"), [])

    def test_section_without_subsection_fails(self) -> None:
        self.assertEqual(len(check_text("src/a.rs", f"// {WORDS[0]} ROADMAP §10\n")), 1)

    def test_cite_on_next_line_fails(self) -> None:
        self.assertEqual(len(check_text("src/a.rs", f"// {WORDS[0]}: gap\n// {CITE}\n")), 1)

    def test_non_words_pass(self) -> None:
        text = f"{WORDS[0]}S\n_{WORDS[0]}_\n{WORDS[0].lower()}\n{WORDS[1].lower()}\nX{WORDS[2]}\n"
        self.assertEqual(check_text("src/a.rs", text), [])

    def test_binary_and_outside_paths_are_skipped(self) -> None:
        marker = f"// {WORDS[1]}\n"
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            for sub in ("src", "user", "docs"):
                (root / sub).mkdir()
            (root / "src" / "a.rs").write_text(marker, encoding="utf-8")
            (root / "user" / "hello.bin").write_bytes(marker.encode() + b"\0\x01")
            (root / "user" / "latin1.txt").write_bytes(marker.encode() + b"\xff\xfe")
            (root / "docs" / "x.md").write_text(marker, encoding="utf-8")
            (root / "Makefile").write_text(marker, encoding="utf-8")
            paths = ["src/a.rs", "user/hello.bin", "user/latin1.txt", "docs/x.md", "Makefile",
                     "src/gone.rs"]
            got = check(root, paths)
        self.assertEqual(got, [f"src/a.rs:1: {WORDS[1]} without a `ROADMAP §N.M` on its line"])

    def test_real_tree_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_todos.main([]), 0)
        self.assertTrue(out.getvalue().startswith("check_todos: ok"))
        self.assertIn("scripts/check_todos.py", check_todos.listed_files())


if __name__ == "__main__":
    unittest.main()
