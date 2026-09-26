"""Host tests for scripts/check_cells.py (cell soundness rules)."""

from __future__ import annotations

import unittest

from scripts import check_cells
from scripts.check_cells import must_be_unsafe_errors

ENTRY = [("src/a.rs", "force_unlock")]


class TestMustBeUnsafe(unittest.TestCase):
    def errs(self, text: str, entries: list[tuple[str, str]] = ENTRY) -> list[str]:
        return must_be_unsafe_errors({"src/a.rs": text}, entries)

    def test_missing_fn_fails(self) -> None:
        self.assertEqual(self.errs("fn other() {}\n"),
                         ["src/a.rs: must-be-unsafe fn force_unlock not found"])

    def test_missing_file_fails(self) -> None:
        self.assertEqual(must_be_unsafe_errors({}, ENTRY),
                         ["src/a.rs: missing file for must-be-unsafe force_unlock"])

    def test_safe_pub_fn_fails(self) -> None:
        self.assertEqual(self.errs("pub fn force_unlock() {}\n"),
                         ["src/a.rs:1: force_unlock must be declared `unsafe fn`"])

    def test_safe_private_fn_fails(self) -> None:
        self.assertEqual(self.errs("\nfn force_unlock(&self) {}\n"),
                         ["src/a.rs:2: force_unlock must be declared `unsafe fn`"])

    def test_unsafe_fns_pass(self) -> None:
        for text in ("pub unsafe fn force_unlock() {}\n",
                     "pub(crate) unsafe fn force_unlock() {}\n",
                     "unsafe fn force_unlock() {}\n",
                     "pub(in crate::x) const unsafe fn force_unlock() {}\n",
                     'pub unsafe extern "C" fn force_unlock() {}\n'):
            with self.subTest(text=text):
                self.assertEqual(self.errs(text), [])

    def test_longer_name_is_not_taken(self) -> None:
        text = "pub fn force_unlock_all() {}\nunsafe fn force_unlock() {}\n"
        self.assertEqual(self.errs(text), [])
        self.assertEqual(self.errs("pub fn force_unlock_all() {}\n"),
                         ["src/a.rs: must-be-unsafe fn force_unlock not found"])

    def test_every_declaration_is_checked(self) -> None:
        text = "unsafe fn force_unlock() {}\nmod m {\n    fn force_unlock() {}\n}\n"
        self.assertEqual(self.errs(text),
                         ["src/a.rs:3: force_unlock must be declared `unsafe fn`"])

    def test_comment_and_call_are_not_declarations(self) -> None:
        text = "// pub fn force_unlock() {}\nfn x() { LOG.force_unlock(); }\n"
        self.assertEqual(self.errs(text),
                         ["src/a.rs: must-be-unsafe fn force_unlock not found"])

    def test_type_qualified_entry(self) -> None:
        entry = [("src/a.rs", "IrqCell::force_unlock")]
        text = ("impl<T> IrqCell<T> {\n    pub unsafe fn force_unlock(&self) {}\n}\n"
                "pub fn force_unlock() {}\n")
        self.assertEqual(self.errs(text, entry), [])
        text = ("impl<T> IrqCell<T> {\n    pub fn force_unlock(&self) {}\n}\n"
                "pub unsafe fn force_unlock() {}\n")
        self.assertEqual(self.errs(text, entry),
                         ["src/a.rs:2: IrqCell::force_unlock must be declared `unsafe fn`"])
        text = "impl Other {\n    pub unsafe fn force_unlock(&self) {}\n}\n"
        self.assertEqual(self.errs(text, entry),
                         ["src/a.rs: must-be-unsafe fn IrqCell::force_unlock not found"])

    def test_list_holds_the_box_functions(self) -> None:
        for entry in [("src/cell.rs", "IrqCell::force_unlock"),
                      ("src/log_init.rs", "force_unlock"),
                      ("src/log_init.rs", "with_logger_unlocked"),
                      ("src/log_init.rs", "dump_tail")]:
            self.assertIn(entry, check_cells.MUST_BE_UNSAFE)

    def test_tree_passes(self) -> None:
        self.assertEqual(must_be_unsafe_errors(check_cells.read_tree()), [])


if __name__ == "__main__":
    unittest.main()
