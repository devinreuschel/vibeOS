"""Host tests for scripts/check_gone.py (removed names stay removed)."""

from __future__ import annotations

import contextlib
import io
import unittest
from unittest import mock

from scripts import check_gone
from scripts.check_gone import check_table, find, scoped_files

KEY = "a box removed it"
ROADMAP = "## Phase 10: C\n- [x] a box removed it\n- [ ] another box\nprose line\n"


def run_find(rows: list[str], files: dict[str, str], extra: list[str] | None = None) -> list[str]:
    return find([(r, KEY) for r in rows], files, sorted([*files, *(extra or [])]))


class TestFind(unittest.TestCase):
    def test_word(self) -> None:
        errs = run_find(["defer_free"], {"src/a.rs": "fn x() {\n    defer_free(p);\n}\n"})
        self.assertEqual(errs, ["src/a.rs:2: 'defer_free' is gone (a box removed it)"])

    def test_word_inside_a_longer_identifier_passes(self) -> None:
        files = {"src/a.rs": "defer_free_all(); undefer_free(); DEFER_FREE;\n"}
        self.assertEqual(run_find(["defer_free"], files), [])

    def test_path_row(self) -> None:
        errs = run_find(["src/old_init.rs"], {"src/old_init.rs": "",
                                              "Makefile": "SRC = src/old_init.rs\n"})
        self.assertEqual(errs, [
            "src/old_init.rs: path 'src/old_init.rs' is gone (a box removed it)",
            "Makefile:1: 'src/old_init.rs' is gone (a box removed it)",
        ])

    def test_directory_row(self) -> None:
        errs = run_find(["src/mm/"], {"src/mm/a.rs": "x\n"})
        self.assertEqual(errs, ["src/mm/a.rs: path 'src/mm/' is gone (a box removed it)"])

    def test_glob_row(self) -> None:
        errs = run_find(["src/*_old.rs"], {"src/x_old.rs": "", "src/x.rs": "src/*_old.rs\n"})
        self.assertEqual(errs, ["src/x_old.rs: path 'src/*_old.rs' is gone (a box removed it)"])

    def test_basename_row(self) -> None:
        errs = run_find(["run_ps2.py"], {"tests/harness/run_ps2.py": "", "a.py": "x\n"})
        self.assertEqual(errs, [
            "tests/harness/run_ps2.py: file named 'run_ps2.py' is gone (a box removed it)",
        ])

    def test_definition_row(self) -> None:
        row = "src/fs/fat_init.rs: fn route"
        files = {"src/fs/fat_init.rs": "pub(crate) fn route(x: u8) {}\nlet route = 1;\n",
                 "src/vfs.rs": "fn route() {}\n"}
        self.assertEqual(run_find([row], files), [
            f"src/fs/fat_init.rs:1: defines fn route: {row!r} is gone (a box removed it)",
        ])
        files["src/fs/fat_init.rs"] = "let route = 1; // route stays a word here\n"
        self.assertEqual(run_find([row], files), [])
        macro = "src/a.rs: macro with_timer"
        self.assertEqual(len(run_find([macro], {"src/a.rs": "macro_rules! with_timer {\n"})), 1)
        py = "tests/harness/h.py: def old"
        self.assertEqual(len(run_find([py], {"tests/harness/h.py": "    def old(self):\n"})), 1)

    def test_makefile_and_workflow_mentions_fail(self) -> None:
        errs = run_find(["test-old"], {"Makefile": "test-old:\n",
                                       ".github/workflows/ci.yml": "run: make test-old\n"})
        self.assertEqual([e.split(":")[0] for e in errs], [".github/workflows/ci.yml", "Makefile"])


class TestTable(unittest.TestCase):
    def test_good_table(self) -> None:
        self.assertEqual(check_table([("x", KEY), ("y", "another box")], ROADMAP), [])

    def test_bad_key(self) -> None:
        errs = check_table([("x", "no such box"), ("y", "prose line"), ("z", "box")], ROADMAP)
        self.assertEqual(len(errs), 3)
        self.assertIn("matches no line", errs[0])
        self.assertIn("not a box", errs[1])
        self.assertIn("matches 2 lines", errs[2])

    def test_duplicate_row(self) -> None:
        errs = check_table([("x", KEY), ("x", "another box")], ROADMAP)
        self.assertEqual(errs, ["GONE: 'x' listed twice"])


class TestTree(unittest.TestCase):
    def test_scope_leaves_out_docs_and_exempt_files(self) -> None:
        files, paths = scoped_files()
        self.assertIn("src/main.rs", paths)
        self.assertIn("Makefile", paths)
        self.assertIn(".github/workflows/ci.yml", paths)
        self.assertNotIn("scripts/check_gone.py", paths)
        self.assertNotIn("tests/harness/test_gone.py", paths)
        self.assertFalse(any(p.startswith("docs/") or p == "CHANGELOG.md" for p in paths))
        # A name only docs/ mentions passes.
        self.assertEqual(find([("Precedence", KEY)], files, paths), [])

    def test_real_tree_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_gone.main([]), 0)
        self.assertIn("check_gone: ok", out.getvalue())

    def test_planted_row_fails(self) -> None:
        key = "`scripts/check_gone.py`, which `make check` runs"
        err = io.StringIO()
        with mock.patch.object(check_gone, "GONE", [("setup_serial_marker_planted", key),
                                                    ("run_e2e.py", key)]), \
                contextlib.redirect_stderr(err):
            self.assertEqual(check_gone.main([]), 1)
        self.assertIn("tests/harness/run_e2e.py: file named 'run_e2e.py'", err.getvalue())
        self.assertNotIn("planted", err.getvalue())


if __name__ == "__main__":
    unittest.main()
