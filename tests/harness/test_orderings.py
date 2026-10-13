"""Host tests for scripts/check_orderings.py (ROADMAP §11.7)."""

from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import check_orderings
from scripts.check_orderings import file_errors, names_pair


def errs(text: str, rel: str = "src/a.rs") -> list[str]:
    e, _n = file_errors(rel, text)
    return e


class TestFileErrors(unittest.TestCase):
    def test_bare_site_fails(self) -> None:
        e = errs("X.store(1, Ordering::Relaxed);\n")
        self.assertEqual(len(e), 1, e)
        self.assertIn("src/a.rs:1: Relaxed without a comment", e[0])

    def test_annotated_passes(self) -> None:
        text = "// Relaxed: a count; pairs with nothing.\nX.store(1, Ordering::Relaxed);\n"
        self.assertEqual(errs(text), [])

    def test_trailing_comment_passes(self) -> None:
        text = "X.store(1, Ordering::Release); // Release: pairs with the Acquire load.\n"
        self.assertEqual(errs(text), [])

    def test_comment_block_above_passes(self) -> None:
        text = (
            "// The flag the waiter spins on.\n"
            "// Acquire: pairs with the Release store in unlock.\n"
            "X.load(Ordering::Acquire);\n"
        )
        self.assertEqual(errs(text), [])

    def test_multiline_statement_uses_comment_above(self) -> None:
        text = (
            "// Release: pairs with the Acquire load in lock.\n"
            "X.store(\n"
            "    1,\n"
            "    Ordering::Release,\n"
            ");\n"
        )
        self.assertEqual(errs(text), [])

    def test_cas_one_line_names_both(self) -> None:
        text = (
            "// AcqRel, Acquire on failure: pairs with the Release store.\n"
            "X.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);\n"
        )
        self.assertEqual(errs(text), [])
        bare = "X.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);\n"
        e = errs(bare)
        self.assertEqual(len(e), 2, e)
        half = (
            "// AcqRel: pairs with the Release store.\n"
            "X.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);\n"
        )
        e = errs(half)
        self.assertEqual(len(e), 1, e)
        self.assertIn("Acquire without a comment", e[0])

    def test_other_statement_comment_does_not_count(self) -> None:
        text = (
            "// Release: pairs with the Acquire load in lock.\n"
            "X.store(1, Ordering::Release);\n"
            "Y.load(Ordering::Acquire);\n"
        )
        e = errs(text)
        self.assertEqual(len(e), 1, e)
        self.assertIn("src/a.rs:3: Acquire", e[0])

    def test_brace_opener_comment_covers_inside(self) -> None:
        text = (
            "// Acquire: pairs with the Release store in unlock.\n"
            "match X.load(Ordering::Acquire) {\n"
            "    _ => {}\n"
            "}\n"
        )
        self.assertEqual(errs(text), [])

    def test_brace_head_with_ordering_is_its_own(self) -> None:
        text = (
            "// Acquire: pairs with the Release store in unlock.\n"
            "if X.load(Ordering::Acquire) {\n"
            "    Y.store(1, Ordering::Release);\n"
            "}\n"
        )
        e = errs(text)
        self.assertEqual(len(e), 1, e)
        self.assertIn("src/a.rs:3: Release", e[0])

    def test_block_continuation_after_brace(self) -> None:
        text = (
            "// Acquire: pairs with the Release store in unlock.\n"
            "unsafe { X }.load(Ordering::Acquire);\n"
        )
        self.assertEqual(errs(text), [])
        text = (
            "if c {\n"
            "} else if X.load(Ordering::Acquire) {\n"
            "}\n"
        )
        e = errs(text)
        self.assertEqual(len(e), 1, e)
        self.assertIn("Acquire without a comment", e[0])

    def test_fence_needs_a_comment(self) -> None:
        e = errs("core::sync::atomic::fence(Ordering::Release);\n")
        self.assertEqual(len(e), 1, e)
        self.assertIn("Release without a comment", e[0])
        text = (
            "// Release: pairs with the Acquire fence in the reader.\n"
            "core::sync::atomic::compiler_fence(Ordering::Release);\n"
        )
        self.assertEqual(errs(text), [])

    def test_seqcst_ignored(self) -> None:
        self.assertEqual(errs("X.store(1, Ordering::SeqCst);\n"), [])

    def test_comments_and_strings_ignored(self) -> None:
        text = (
            "// X.load(Ordering::Acquire);\n"
            'let s = "Ordering::Release";\n'
            "fn f() {}\n"
        )
        self.assertEqual(errs(text), [])

    def test_atomic_ordering_path(self) -> None:
        e = errs("X.store(1, AtomicOrdering::Acquire);\n")
        self.assertEqual(len(e), 1, e)
        text = (
            "// Acquire: pairs with the Release store in unlock.\n"
            "X.store(1, AtomicOrdering::Acquire);\n"
        )
        self.assertEqual(errs(text), [])

    def test_cfg_test_item_skipped(self) -> None:
        text = (
            "#[cfg(test)]\n"
            "fn f() {\n"
            "    X.load(Ordering::Acquire);\n"
            "}\n"
            "X.store(1, Ordering::Relaxed);\n"
        )
        e = errs(text)
        self.assertEqual(len(e), 1, e)
        self.assertIn("Relaxed", e[0])

    def test_bare_import_fails(self) -> None:
        e = errs("use core::sync::atomic::Ordering::{Acquire, Release};\n")
        self.assertEqual(len(e), 1, e)
        self.assertIn("bare name", e[0])
        e = errs("use core::sync::atomic::Ordering::*;\n")
        self.assertEqual(len(e), 1, e)

    def test_rename_must_end_in_ordering(self) -> None:
        e = errs("use core::sync::atomic::Ordering as Ord;\n")
        self.assertEqual(len(e), 1, e)
        self.assertIn("ending in `Ordering`", e[0])
        self.assertEqual(
            errs("use core::sync::atomic::Ordering as AtomicOrdering;\n"),
            [],
        )

    def test_one_line_rule(self) -> None:
        text = (
            "// Acquire:\n"
            "// pairs with the Release store in unlock.\n"
            "X.load(Ordering::Acquire);\n"
        )
        e = errs(text)
        self.assertEqual(len(e), 1, e)
        self.assertFalse(names_pair(["// Acquire:", "// pairs with x."], "Acquire"))
        self.assertTrue(names_pair(["// Acquire: pairs with x."], "Acquire"))


class TestCheckAndMain(unittest.TestCase):
    def write_tree(self, root: Path, files: dict[str, str]) -> None:
        for rel, text in files.items():
            p = root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)

    def test_ktest_file_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            self.write_tree(root, {
                "src/foo/ktest.rs": "X.load(Ordering::Acquire);\n",
                "src/a.rs": (
                    "// Relaxed: a count; pairs with nothing.\n"
                    "X.store(1, Ordering::Relaxed);\n"
                ),
            })
            e, n = check_orderings.check(root)
            self.assertEqual(e, [])
            self.assertEqual(n, 1)

    def test_main_temp_tree(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            self.write_tree(root, {"src/a.rs": "X.store(1, Ordering::Relaxed);\n"})
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = check_orderings.main(["--root", d])
            self.assertEqual(rc, 1)
            self.assertIn("src/a.rs:1: Relaxed without a comment", out.getvalue())
            (root / "src/a.rs").write_text(
                "// Relaxed: a count; pairs with nothing.\n"
                "X.store(1, Ordering::Relaxed);\n",
            )
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = check_orderings.main(["--root", d])
            self.assertEqual(rc, 0)
            self.assertIn("check_orderings: ok (1 orderings)", out.getvalue())

    def test_real_tree_passes(self) -> None:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = check_orderings.main([])
        self.assertEqual(rc, 0, err.getvalue() + out.getvalue())
        self.assertRegex(out.getvalue(), r"check_orderings: ok \(\d+ orderings\)\n")


if __name__ == "__main__":
    unittest.main()
