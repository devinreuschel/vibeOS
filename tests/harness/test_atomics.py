"""Host tests for scripts/check_atomics.py (C-ATOMICS, ROADMAP §10.8)."""

from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import check_atomics
from scripts.check_atomics import check_tree, file_errors

BASE = {
    "crates/core/src/lib.rs": "pub mod atomic;\npub mod a;\n",
    "crates/core/src/atomic.rs": "pub use core::sync::atomic::{AtomicU32, Ordering};\n",
    "crates/core/src/a/mod.rs": "use crate::atomic::AtomicU32;\n",
}


def run_tree(extra: dict[str, str], pending: tuple[str, ...] = ()) -> tuple[list[str], list[str]]:
    with tempfile.TemporaryDirectory() as t:
        root = Path(t)
        for rel, text in {**BASE, **extra}.items():
            p = root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        return check_tree(root, pending)


class TestFileErrors(unittest.TestCase):
    def test_banned_paths(self) -> None:
        for line in ("use core::sync::atomic::AtomicU32;", "use std::sync::atomic::Ordering;",
                     "core::hint::spin_loop();", "std::hint::spin_loop();",
                     "static X: core::sync::atomic::AtomicU8 = X::new(0);"):
            with self.subTest(line=line):
                errs = file_errors("crates/core/src/x.rs", line + "\n", False)
                self.assertEqual(len(errs), 1, errs)
                self.assertIn("crates/core/src/x.rs:1:", errs[0])

    def test_seam_comments_and_other_paths_pass(self) -> None:
        self.assertEqual(file_errors("crates/core/src/atomic.rs",
                                     "pub use core::sync::atomic::AtomicU8;\n", False), [])
        text = ("use crate::atomic::{AtomicU8, spin_loop};\n"
                "// core::sync::atomic is banned here\n"
                "use core::hint::black_box;\n")
        self.assertEqual(file_errors("crates/core/src/x.rs", text, False), [])

    def test_inline_test_modules(self) -> None:
        cfgs = ("#[cfg(test)]", "#[cfg(all(test, loom))]", '#[cfg(all(test, feature = "std"))]')
        for cfg in cfgs:
            with self.subTest(cfg=cfg):
                text = f"{cfg}\nmod tests {{\n    use core::sync::atomic::AtomicU8;\n}}\n"
                self.assertEqual(file_errors("crates/core/src/x.rs", text, False), [])
        for cfg in ("#[cfg(not(test))]", "#[cfg(any(test, loom))]"):
            with self.subTest(cfg=cfg):
                text = f"{cfg}\nmod tests {{\n    use core::sync::atomic::AtomicU8;\n}}\n"
                self.assertEqual(len(file_errors("crates/core/src/x.rs", text, False)), 1)

    def test_code_after_a_test_module_is_checked(self) -> None:
        text = ("#[cfg(test)]\nmod tests {\n    fn f() {\n    }\n}\n"
                "use core::sync::atomic::AtomicU8;\n")
        errs = file_errors("crates/core/src/x.rs", text, False)
        self.assertEqual(len(errs), 1, errs)
        self.assertIn(":6:", errs[0])

    def test_layout_fixed_repr_c_field(self) -> None:
        head = "#[repr(C)]\npub struct PerCpu {\n    pub self_ptr: *mut PerCpu,\n"
        field = "    pub irq_nest: core::sync::atomic::AtomicU32,\n}\n"
        pinned = "const _: () = assert!(offset_of!(PerCpu, irq_nest) == 8);\n"
        self.assertEqual(file_errors("crates/core/src/x.rs", head + field + pinned, False), [])
        self.assertEqual(len(file_errors("crates/core/src/x.rs", head + field, False)), 1)
        plain = head.replace("#[repr(C)]\n", "") + field + pinned
        self.assertEqual(len(file_errors("crates/core/src/x.rs", plain, False)), 1)

    def test_test_file_passes(self) -> None:
        text = "use core::sync::atomic::X;\n"
        self.assertEqual(file_errors("crates/core/src/t.rs", text, True), [])


class TestCheckTree(unittest.TestCase):
    def test_clean(self) -> None:
        self.assertEqual(run_tree({}), ([], []))

    def test_declared_test_files_are_test_code(self) -> None:
        extra = {
            "crates/core/src/a/mod.rs": "#[cfg(test)]\nmod tests;\n#[cfg(test)]\nmod helpers;\n",
            "crates/core/src/a/tests.rs": "mod deep;\nuse std::sync::atomic::AtomicU8;\n",
            "crates/core/src/a/tests/deep.rs": "use core::sync::atomic::AtomicU8;\n",
            "crates/core/src/a/helpers/mod.rs": "use core::sync::atomic::AtomicU8;\n",
        }
        self.assertEqual(run_tree(extra), ([], []))

    def test_undeclared_or_non_test_file_fails(self) -> None:
        extra = {
            "crates/core/src/a/mod.rs": "#[cfg(any(test, loom))]\nmod b;\n",
            "crates/core/src/a/b.rs": "use core::sync::atomic::AtomicU8;\n",
        }
        errors, failing = run_tree(extra)
        self.assertEqual(failing, ["crates/core/src/a/b.rs"])
        self.assertEqual(len(errors), 1)

    def test_pending(self) -> None:
        bad = {"crates/core/src/a/mod.rs": "use core::sync::atomic::AtomicU8;\n"}
        self.assertEqual(run_tree(bad, ("crates/core/src/a/mod.rs",)),
                         ([], ["crates/core/src/a/mod.rs"]))
        errors, _ = run_tree({}, ("crates/core/src/a/mod.rs",))
        self.assertIn("passes; remove the entry", errors[0])
        errors, _ = run_tree({}, ("crates/core/src/gone.rs",))
        self.assertIn("missing", errors[0])


class TestMain(unittest.TestCase):
    def test_real_tree_passes(self) -> None:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = check_atomics.main([])
        self.assertEqual(rc, 0, err.getvalue())

    def test_pending_is_sorted_and_unique(self) -> None:
        self.assertEqual(list(check_atomics.PENDING), sorted(set(check_atomics.PENDING)))


if __name__ == "__main__":
    unittest.main()
