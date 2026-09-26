"""Host tests for scripts/check_cells.py (cell soundness rules)."""

from __future__ import annotations

import unittest
from pathlib import Path

from scripts import check_cells
from scripts.check_cells import impl_errors, must_be_unsafe_errors, unsafe_impls

ENTRY = [("src/a.rs", "force_unlock")]
FIXTURES = Path(__file__).resolve().parent / "fixtures" / "cells"


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


class TestImplHeaders(unittest.TestCase):
    def errs(self, text: str, path: str = "src/cell.rs") -> list[str]:
        return impl_errors(path, text)

    def test_one_line(self) -> None:
        self.assertEqual(self.errs("unsafe impl<T: Send> Sync for IrqCell<T> {}\n"), [])
        self.assertEqual(self.errs("unsafe impl<T> Sync for IrqCell<T> {}\n"),
                         ["src/cell.rs:1: unsafe impl Sync for IrqCell: T is not bounded by Send"])

    def test_multi_line_with_where_on_its_own_line(self) -> None:
        text = ("unsafe impl<T>\n"
                "    Send for IrqCell<T>\n"
                "where\n"
                "    T: Send,\n"
                "{\n}\n")
        self.assertEqual(self.errs(text), [])
        self.assertEqual(self.errs(text.replace("T: Send,", "T: Copy,")),
                         ["src/cell.rs:1: unsafe impl Send for IrqCell: T is not bounded by Send"])

    def test_where_adds_to_inline_bounds(self) -> None:
        text = "unsafe impl<T: Send> Sync for RwLock<T> where T: Sync {}\n"
        self.assertEqual(self.errs(text, "src/sync_init.rs"), [])

    def test_sized_alone_is_no_bound(self) -> None:
        self.assertEqual(self.errs("unsafe impl<T: ?Sized> Send for IrqCell<T> {}\n"),
                         ["src/cell.rs:1: unsafe impl Send for IrqCell: T is not bounded by Send"])
        text = "unsafe impl<T: ?Sized + Send> Send for IrqCell<T> {}\n"
        self.assertEqual(self.errs(text), [])

    def test_two_parameters_name_the_unbounded_one(self) -> None:
        text = "unsafe impl<A: Send, B> Send for Pair<A, B> {}\n"
        self.assertEqual(self.errs(text, "src/sync_init.rs"), [
            "src/sync_init.rs:1: unsafe impl Send for Pair: B is not bounded by Send"])

    def test_const_generic_and_lifetimes_are_not_type_parameters(self) -> None:
        for text in ("unsafe impl<T: Send, const N: usize> Sync for Channel<T, N> {}\n",
                     "unsafe impl<'a, T: Send + 'a> Send for Guard<'a, T> {}\n",
                     "unsafe impl<const N: usize> Sync for Ring<N> {}\n"):
            with self.subTest(text=text):
                self.assertEqual(self.errs(text, "src/sync_init.rs"), [])

    def test_shares_ref_needs_sync(self) -> None:
        for ty in ("BootCell", "RwLock"):
            with self.subTest(ty=ty):
                self.assertEqual(
                    self.errs(f"unsafe impl<T: Send> Sync for {ty}<T> {{}}\n"),
                    [f"src/cell.rs:1: unsafe impl Sync for {ty}: T is not bounded by Sync"])
                self.assertEqual(
                    self.errs(f"unsafe impl<T: Send + Sync> Sync for {ty}<T> {{}}\n"), [])
                self.assertEqual(self.errs(f"unsafe impl<T: Send> Send for {ty}<T> {{}}\n"), [])

    def test_generic_impl_location(self) -> None:
        text = "unsafe impl<T: Send> Sync for MyCell<T> {}\n"
        for path in check_cells.GENERIC_IMPL_FILES:
            with self.subTest(path=path):
                self.assertEqual(self.errs(text, path), [])
        self.assertEqual(self.errs(text, "src/foo.rs"), [
            "src/foo.rs:1: unsafe impl Sync for MyCell: a generic impl belongs only in "
            + ", ".join(check_cells.GENERIC_IMPL_FILES)])

    def test_concrete_impl_passes_anywhere(self) -> None:
        text = ("// SAFETY: invariant I120, established at `per_cpu_init::cpu`.\n"
                "unsafe impl Sync for PerCpu {}\n")
        self.assertEqual(self.errs(text, "src/per_cpu.rs"), [])

    def test_comment_inside_a_header(self) -> None:
        text = ("unsafe impl<T /* no { here */: Send> // a { comment\n"
                "    Sync for IrqCell<T> {}\n")
        self.assertEqual(self.errs(text), [])
        text = "unsafe impl<T /* : Send */> Sync for IrqCell<T> {}\n"
        self.assertEqual(len(self.errs(text)), 1)

    def test_spin_mutex_guard_form(self) -> None:
        text = "unsafe impl<T: Send + Sync> Sync for SpinMutexGuard<'_, T> {}\n"
        self.assertEqual(self.errs(text, "src/sync_init.rs"), [])

    def test_try_arc_form(self) -> None:
        for trait in ("Send", "Sync"):
            text = f"unsafe impl<T: ?Sized + Send + Sync> {trait} for TryArc<T> {{}}\n"
            self.assertEqual(self.errs(text, "src/kalloc.rs"), [])

    def test_other_traits_are_ignored(self) -> None:
        text = "unsafe impl<A: FrameAlloc> FrameAlloc for Counting<'_, A> {}\n"
        self.assertEqual(self.errs(text, "src/addr_space.rs"), [])

    def test_header_fields(self) -> None:
        text = ("unsafe impl<'a, T: ?Sized + Send, const N: usize>\n"
                "  core::marker::Sync for a::B<'a, T, N> where T: Sync {}\n")
        [imp] = unsafe_impls(text)
        self.assertEqual((imp.line, imp.trait, imp.self_ty), (1, "Sync", "B"))
        self.assertEqual(imp.params, (("T", "?Sized + Send"),))
        self.assertEqual(imp.where, (("T", "Sync"),))

    def test_fixture_cell_869b6da(self) -> None:
        text = (FIXTURES / "cell_869b6da.rs").read_text(encoding="utf-8")
        errs = self.errs(text, "src/cell.rs")
        self.assertEqual([e.split(":")[1] for e in errs], ["37", "38", "94", "95"])

    def test_fixture_sync_init_869b6da(self) -> None:
        text = (FIXTURES / "sync_init_869b6da.rs").read_text(encoding="utf-8")
        impls = unsafe_impls(text)
        self.assertEqual(sum(1 for i in impls if i.params), 8)
        self.assertEqual(sum(1 for i in impls if not i.params), 4)
        self.assertEqual(self.errs(text, "src/sync_init.rs"), [])

    def test_tree_passes(self) -> None:
        for path, text in check_cells.read_tree().items():
            with self.subTest(path=path):
                self.assertEqual(impl_errors(path, text), [])


if __name__ == "__main__":
    unittest.main()
