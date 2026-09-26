"""Host tests for scripts/check_cells.py (cell soundness rules)."""

from __future__ import annotations

import unittest

from scripts import check_cells
from scripts.check_cells import impl_errors, must_be_unsafe_errors, unsafe_impls

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
                      ("src/log_init.rs", "dump_tail"),
                      ("src/per_cpu_init.rs", "with_cpu")]:
            self.assertIn(entry, check_cells.MUST_BE_UNSAFE)

    def test_tree_passes(self) -> None:
        self.assertEqual(must_be_unsafe_errors(check_cells.read_tree()), [])


class TestRemoteView(unittest.TestCase):
    def test_view_is_listed(self) -> None:
        self.assertIn("PerCpuRemote", check_cells.NO_UNSAFE_IMPL)

    def test_unsafe_impl_for_view_fails_anywhere(self) -> None:
        for trait in ("Send", "Sync"):
            text = f"// SAFETY: no.\nunsafe impl {trait} for PerCpuRemote {{}}\n"
            for path in ("src/per_cpu.rs", "src/cell.rs", "src/x.rs"):
                with self.subTest(trait=trait, path=path):
                    self.assertEqual(impl_errors(path, text), [
                        f"{path}:2: unsafe impl {trait} for PerCpuRemote: "
                        f"PerCpuRemote must be {trait} from its fields alone",
                    ])

    def test_split_and_qualified_header_fails(self) -> None:
        text = "unsafe impl\n    core::marker::Sync\n    for vibeos::per_cpu::PerCpuRemote\n{\n}\n"
        self.assertEqual(len(impl_errors("src/a.rs", text)), 1)

    def test_other_impls_of_the_view_pass(self) -> None:
        text = ("impl Default for PerCpuRemote {}\n"
                "unsafe impl Send for PerCpu {}\n"
                "// unsafe impl Sync for PerCpuRemote {}\n")
        self.assertEqual(impl_errors("src/per_cpu.rs", text), [])

    def test_with_cpu_must_be_unsafe(self) -> None:
        entry = [("src/per_cpu_init.rs", "with_cpu")]
        safe = "pub fn with_cpu<R>(id: u32, f: impl FnOnce(&mut PerCpu) -> R) -> Option<R> {}\n"
        self.assertEqual(must_be_unsafe_errors({"src/per_cpu_init.rs": safe}, entry),
                         ["src/per_cpu_init.rs:1: with_cpu must be declared `unsafe fn`"])
        text = safe.replace("pub fn", "pub unsafe fn")
        self.assertEqual(must_be_unsafe_errors({"src/per_cpu_init.rs": text}, entry), [])

    def test_tree_has_no_view_impl(self) -> None:
        for path, text in check_cells.read_tree().items():
            self.assertEqual(
                [i for i in unsafe_impls(text) if i.self_ty == "PerCpuRemote"], [], path)


if __name__ == "__main__":
    unittest.main()
