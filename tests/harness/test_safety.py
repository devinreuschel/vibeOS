"""Host tests for scripts/check_safety.py (AGENTS.md rule 7, ROADMAP §10.1)."""

from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import check_safety
from scripts.check_safety import ModuleIndex, check_tree, register_ids, resolves, safety_comments

TREE = {
    "src/main.rs": "mod a;\nmod cell;\nuse a::b as bb;\nuse a::{b::T as Tee, c};\n",
    "src/a/mod.rs": "pub mod b;\n#[path = \"odd_name.rs\"]\npub mod c;\n",
    "src/a/b.rs": (
        "pub fn f() {}\n"
        "pub struct T;\n"
        "impl T {\n"
        "    pub fn m(&self) {}\n"
        "    const K: u8 = 0;\n"
        "}\n"
        "unsafe impl Sync for T {}\n"
        "static S: u8 = 0;\n"
        "fn g() {\n"
        "    // SAFETY: invariant I1; established by `a::b::f`.\n"
        "    unsafe {}\n"
        "}\n"
    ),
    "src/a/odd_name.rs": "pub(crate) const C: u32 = 1;\nmod testing {\n    pub fn inner() {}\n}\n",
    "src/cell.rs": (
        "pub struct BootCell;\nimpl<T> Trait for BootCell<T> {\n    fn set(&self) {}\n}\n"
    ),
    "crates/core/src/lib.rs": "pub mod mm;\n",
    "crates/core/src/mm/mod.rs": "pub mod pmm;\n",
    "crates/core/src/mm/pmm.rs": (
        "pub struct Buddy;\nimpl Buddy {\n    pub fn insert_region(&mut self) {}\n}\n"
    ),
    "docs/INVARIANTS.md": "| # | Invariant |\n|---|---|\n| I1 | one |\n| I22 | two |\n",
}


def make_tree(root: Path, extra: dict[str, str] | None = None) -> None:
    for rel, text in {**TREE, **(extra or {})}.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)


class TestSafetyComments(unittest.TestCase):
    def test_comment_takes_following_slash_lines(self) -> None:
        text = ("x();\n    // SAFETY: invariant I1, established\n    // by `a::b::f`.\n"
                "    unsafe {}\n// SAFETY: here.\n/// doc\n")
        cs = safety_comments(text)
        self.assertEqual([c.line for c in cs], [2, 5])
        self.assertIn("by `a::b::f`.", cs[0].text)
        self.assertNotIn("doc", cs[1].text)

    def test_adjacent_safety_comments_split(self) -> None:
        cs = safety_comments("// SAFETY: one\n// SAFETY: two\n")
        self.assertEqual([c.text for c in cs], ["SAFETY: one", "SAFETY: two"])


class TestRegisterIds(unittest.TestCase):
    def test_rows_only(self) -> None:
        self.assertEqual(register_ids("| I1 | x |\n|I22| y |\nsee I3\n| # | I4 |\n"), {"I1", "I22"})


class TestResolves(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        make_tree(self.root)
        self.index = ModuleIndex(self.root)
        self.kmain = self.index.module_of(self.root / "src/main.rs")
        self.kb = self.index.module_of(self.root / "src/a/b.rs")
        self.core = self.index.module_of(self.root / "crates/core/src/mm/pmm.rs")

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_modules_found_with_path_attr(self) -> None:
        self.assertIsNotNone(self.index.module_of(self.root / "src/a/odd_name.rs"))
        assert self.kmain is not None and self.kb is not None and self.core is not None

    def check(self, frm: object, path: str, want: bool) -> None:
        with self.subTest(path=path):
            self.assertEqual(resolves(self.index, frm, path), want)  # type: ignore[arg-type]

    def test_items_from_crate_root(self) -> None:
        for p in ("a::b::f", "a::b::T", "a::b::S", "a::c::C", "crate::a::b::f", "cell::BootCell"):
            self.check(self.kb, p, True)

    def test_impl_items(self) -> None:
        self.check(self.kb, "a::b::T::m", True)
        self.check(self.kb, "a::b::T::K", True)
        self.check(self.kb, "cell::BootCell::set", True)
        self.check(self.kb, "a::b::T::nope", False)

    def test_aliases(self) -> None:
        self.check(self.kb, "bb::f", True)
        self.check(self.kb, "c::C", True)
        self.check(self.kb, "Tee::m", True)

    def test_self_super_and_vibeos(self) -> None:
        self.check(self.kb, "self::f", True)
        self.check(self.kb, "super::c::C", True)
        self.check(self.kb, "vibeos::mm::pmm::Buddy::insert_region", True)
        self.check(self.core, "crate::mm::pmm::Buddy", True)
        self.check(self.core, "mm::pmm::Buddy::insert_region", True)

    def test_unresolved(self) -> None:
        for p in ("a::b", "a::b::missing", "b::f", "a::c::testing::inner", "core::ptr::null",
                  "pmm::Buddy", "super::super::x"):
            self.check(self.kb, p, False)


class TestCheckTree(unittest.TestCase):
    def run_tree(self, extra: dict[str, str], pending: tuple[str, ...] = (),
                 stale: tuple[tuple[str, str], ...] = ()) -> tuple[list[str], list[str]]:
        with tempfile.TemporaryDirectory() as t:
            root = Path(t)
            make_tree(root, extra)
            return check_tree(root, pending, stale)

    def test_clean_tree(self) -> None:
        errors, failing = self.run_tree({})
        self.assertEqual((errors, failing), ([], []))

    def test_here_passes(self) -> None:
        errors, _ = self.run_tree({"src/a/c2.rs": "// SAFETY: invariant I22, established here.\n"})
        self.assertEqual(errors, [])

    def test_unresolved_path_fails(self) -> None:
        extra = {"src/a/b.rs": TREE["src/a/b.rs"] + "// SAFETY: caller guarantees `x::y`.\n"}
        errors, failing = self.run_tree(extra)
        self.assertEqual(failing, ["src/a/b.rs"])
        self.assertIn("src/a/b.rs:13:", errors[0])
        self.assertIn("x::y", errors[0])

    def test_missing_register_row_fails(self) -> None:
        comment = "// SAFETY: invariants I1 and I9, established here.\n"
        extra = {"src/a/b.rs": TREE["src/a/b.rs"] + comment}
        errors, _ = self.run_tree(extra)
        self.assertEqual(len(errors), 1)
        self.assertIn("invariant I9 has no row", errors[0])

    def test_pending_skips_and_stale_entry_fails(self) -> None:
        bad = {"src/a/b.rs": TREE["src/a/b.rs"] + "// SAFETY: trust me.\n"}
        errors, failing = self.run_tree(bad, pending=("src/a/b.rs",))
        self.assertEqual((errors, failing), ([], ["src/a/b.rs"]))
        errors, _ = self.run_tree({}, pending=("src/a/b.rs",))
        self.assertEqual(len(errors), 1)
        self.assertIn("passes; remove the entry", errors[0])
        errors, _ = self.run_tree({}, pending=("src/gone.rs",))
        self.assertIn("missing", errors[0])


class TestMain(unittest.TestCase):
    def test_real_tree_passes(self) -> None:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = check_safety.main([])
        self.assertEqual(rc, 0, err.getvalue())

    def test_pending_is_sorted_and_unique(self) -> None:
        self.assertEqual(list(check_safety.PENDING), sorted(set(check_safety.PENDING)))


if __name__ == "__main__":
    unittest.main()
