"""Host tests for scripts/check_limits.py (ROADMAP §10.4, D1)."""

from __future__ import annotations

import unittest

from scripts.check_limits import (
    ALLOW,
    LIMITS,
    ROOT,
    TABLES,
    Allow,
    check,
    consts,
    gate_values,
    int_value,
    source_files,
)

GATE = (
    "- [ ] the growable tables are heap-sized at init from one `limits` module, and this gate "
    "states the limits Phase 12 is tested against: 256 processes, 256 descriptors per process, "
    "1024 threads, 1024 open files, 1024 inodes, 1024 dentries, 16 mounts, and 256 regions per "
    "address space until §12.4's region tree"
)

VALUES = {
    "MAX_PROCS": 256,
    "MAX_FDS": 256,
    "MAX_THREADS": 1024,
    "MAX_OPEN_FILES": 1024,
    "MAX_INODES": 1024,
    "MAX_DENTRIES": 1024,
    "MAX_MOUNTS": 16,
    "MAX_REGIONS": 256,
}


def limits_rs(values: dict[str, int] = VALUES) -> str:
    return "".join(f"pub const {n}: usize = {v};\n" for n, v in values.items())


def tree(**extra: str) -> dict[str, str]:
    files = {LIMITS: limits_rs()}
    files.update({k.replace("__", "/").replace("_rs", ".rs"): v for k, v in extra.items()})
    return files


class TestConsts(unittest.TestCase):
    def test_visibility_types_and_fn_local(self) -> None:
        text = (
            "pub const MAX_A: usize = 1;\n"
            "pub(crate) const MAX_B: u32 = 2;\n"
            "pub(super) const MAX_C: u64 = 1 << 3;\n"
            "const MAX_D: u8 = 4;\n"
            "fn f() {\n    const MAX_E: usize = 5;\n}\n"
            "// const MAX_F: usize = 6;\n"
            "const NOT_MAX: usize = 7;\n"
            "static MAX_G: usize = 8;\n"
        )
        got = consts(text)
        self.assertEqual(sorted(got), ["MAX_A", "MAX_B", "MAX_C", "MAX_D", "MAX_E"])
        self.assertEqual(got["MAX_C"], "1 << 3")

    def test_int_value(self) -> None:
        self.assertEqual(int_value("1_024"), 1024)
        self.assertEqual(int_value("0x10"), 16)
        self.assertIsNone(int_value("2 * MAX_X"))


class TestAllow(unittest.TestCase):
    def test_listed_passes_and_unlisted_fails(self) -> None:
        files = tree(src__a_rs="pub const MAX_X: usize = 3;\npub const MAX_Y: usize = 4;\n")
        allow = [Allow("src/a.rs", "MAX_X", "hardware", "the device's 3 queues")]
        errs = check(files, allow, GATE)
        self.assertEqual(len(errs), 1, errs)
        self.assertIn("src/a.rs: const MAX_Y outside the limits module", errs[0])

    def test_limits_module_needs_no_entry(self) -> None:
        self.assertEqual(check(tree(), [], GATE), [])

    def test_empty_bound_and_bad_kind_fail(self) -> None:
        files = tree(src__a_rs="const MAX_X: usize = 3;\n")
        errs = check(files, [Allow("src/a.rs", "MAX_X", "hardware", " ")], GATE)
        self.assertEqual(len(errs), 1)
        self.assertIn("no bound", errs[0])
        errs = check(files, [Allow("src/a.rs", "MAX_X", "policy", "a bound")], GATE)
        self.assertEqual(len(errs), 1)
        self.assertIn("not hardware or on-disk", errs[0])

    def test_stale_entry_fails(self) -> None:
        errs = check(tree(), [Allow("src/gone.rs", "MAX_X", "on-disk", "a format")], GATE)
        self.assertEqual(len(errs), 1)
        self.assertIn("no such constant", errs[0])

    def test_duplicate_entry_fails(self) -> None:
        files = tree(src__a_rs="const MAX_X: usize = 3;\n")
        a = Allow("src/a.rs", "MAX_X", "hardware", "a bound")
        self.assertIn("listed twice", check(files, [a, a], GATE)[0])


class TestTableArrays(unittest.TestCase):
    def test_array_sized_by_a_table_constant_fails(self) -> None:
        for body in (
            "use crate::limits::MAX_THREADS;\nstatic A: [u32; MAX_THREADS] = [0; 4];\n",
            "fn f() { let a = [0u8; limits::MAX_FDS]; }\n",
            "struct S { a: [u8; vibeos::limits::MAX_REGIONS ] }\n",
        ):
            errs = check(tree(src__a_rs=body), [], GATE)
            self.assertEqual(len(errs), 1, (body, errs))
            self.assertIn("an array sized by", errs[0])

    def test_other_lengths_and_comments_pass(self) -> None:
        body = (
            "const N: usize = 4;\nstatic A: [u32; N] = [0; N];\n"
            "// [u32; MAX_THREADS] was the old shape\n"
            "fn f(n: usize) -> bool { n < MAX_THREADS }\n"
        )
        self.assertEqual(check(tree(src__a_rs=body), [], GATE), [])

    def test_a_module_s_own_constant_is_exempt(self) -> None:
        # vibefs's own MAX_INODES, an on-disk count, defined in its mod.rs.
        allow = [Allow("crates/core/src/fs/vibefs/mod.rs", "MAX_INODES", "on-disk", "VIBEFS §3")]
        files = tree()
        files["crates/core/src/fs/vibefs/mod.rs"] = (
            "pub const MAX_INODES: usize = 64;\nstruct V { inodes: [u8; MAX_INODES] }\n")
        files["crates/core/src/fs/vibefs/commit.rs"] = "fn f() { let a = [0usize; MAX_INODES]; }\n"
        self.assertEqual(check(files, allow, GATE), [])
        # The limits one, named through its module, is not.
        files["crates/core/src/fs/vibefs/fsck.rs"] = (
            "fn f() { let a = [0u8; limits::MAX_INODES]; }\n")
        self.assertEqual(len(check(files, allow, GATE)), 1)


class TestGateValues(unittest.TestCase):
    def test_gate_line_parses(self) -> None:
        self.assertEqual(gate_values(GATE), VALUES)
        self.assertIsNone(gate_values("no gate here"))

    def test_mismatch_fails(self) -> None:
        values = dict(VALUES, MAX_THREADS=64)
        files = {LIMITS: limits_rs(values)}
        errs = check(files, [], GATE)
        self.assertEqual(len(errs), 1)
        self.assertIn("MAX_THREADS is 64, the exit gate states 1024 threads", errs[0])

    def test_missing_gate_line_fails(self) -> None:
        errs = check(tree(), [], "the gate says nothing")
        self.assertEqual(len(errs), 1)
        self.assertIn("no exit-gate line", errs[0])

    def test_every_table_is_on_the_gate_line(self) -> None:
        self.assertEqual(sorted(TABLES), sorted(VALUES))


class TestTree(unittest.TestCase):
    def test_real_tree_passes(self) -> None:
        roadmap = (ROOT / "docs" / "ROADMAP.md").read_text(encoding="utf-8")
        self.assertEqual(check(source_files(ROOT), ALLOW, roadmap), [])


if __name__ == "__main__":
    unittest.main()
