"""Host tests for scripts/check_cycles.py (ROADMAP §10.3, A4)."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts.check_cycles import ROOT, build, check, failures, tokenize

RAW_OK = {
    "src/log/mod.rs": "pub mod serial;\n",
    "src/log/serial/mod.rs": "pub mod raw;\n",
    "src/log/serial/raw.rs": "use crate::x86;\npub fn w() { x86::outb(); }\n",
    "src/arch/mod.rs": "pub mod cpu;\n",
    "src/arch/cpu.rs": "pub fn outb() {}\n",
}
KERNEL_ROOT = "mod arch;\nmod log;\nuse arch::cpu as x86;\n"


class Tree:
    """A fixture tree: `src/main.rs` and `crates/core/src/lib.rs` plus `files`."""

    def __init__(self, kernel_root: str, files: dict[str, str], core_root: str = "",
                 raw: bool = True) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        all_files = dict(RAW_OK) if raw else {}
        all_files.update(files)
        root_src = kernel_root
        if raw:
            root_src = KERNEL_ROOT + kernel_root
        all_files["src/main.rs"] = root_src
        all_files["crates/core/src/lib.rs"] = core_root
        for rel, text in all_files.items():
            p = self.root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text, encoding="utf-8")

    def keys(self) -> set[str]:
        g, kernel, _core, res = build(self.root)
        return set(failures(g, kernel, res))

    def close(self) -> None:
        self.tmp.cleanup()


def keys(kernel_root: str, files: dict[str, str], core_root: str = "",
         raw: bool = True) -> set[str]:
    t = Tree(kernel_root, files, core_root, raw)
    try:
        return t.keys()
    finally:
        t.close()


def two_way(a: str, b: str) -> str:
    x, y = sorted((a, b))
    return f"two-way {x} {y}"


class TestTokenize(unittest.TestCase):
    def test_comments_and_literals_drop(self) -> None:
        toks = tokenize('// crate::a\n/* crate::b /* nested */ */ let s = "crate::c"; \'x\'')
        ids = [t.text for t in toks if t.kind == "id"]
        self.assertEqual(ids, ["let", "s"])

    def test_raw_string_and_lifetime(self) -> None:
        toks = tokenize("fn f<'a>(x: &'a u8) { asm!(r#\"call {crate::b}\"#) }")
        ids = [t.text for t in toks if t.kind == "id"]
        self.assertNotIn("crate", ids)
        self.assertIn("asm", ids)

    def test_lines(self) -> None:
        toks = tokenize("a\n\"x\ny\"\nb")
        self.assertEqual([(t.text, t.line) for t in toks if t.kind == "id"], [("a", 1), ("b", 4)])


class TestTwoWay(unittest.TestCase):
    ROOT_AB = "mod a;\nmod b;\n"

    def test_use_pair(self) -> None:
        got = keys(self.ROOT_AB, {"src/a.rs": "use crate::b;\n", "src/b.rs": "use crate::a;\n"})
        self.assertEqual(got, {two_way("kernel:a", "kernel:b")})

    def test_expression_pair(self) -> None:
        got = keys(self.ROOT_AB, {"src/a.rs": "fn f() { crate::b::g(); }\n",
                                  "src/b.rs": "fn g() { crate::a::f(); }\n"})
        self.assertEqual(got, {two_way("kernel:a", "kernel:b")})

    def test_nested_use_tree(self) -> None:
        got = keys("mod a;\nmod b;\nmod c;\n", {
            "src/a.rs": "use crate::{b, c::{self as cc, X}};\n",
            "src/b.rs": "",
            "src/c.rs": "pub struct X;\nuse crate::a::f;\n",
        })
        self.assertEqual(got, {two_way("kernel:a", "kernel:c")})

    def test_super_siblings(self) -> None:
        got = keys("mod d;\n", {
            "src/d/mod.rs": "mod a;\nmod b;\n",
            "src/d/a.rs": "use super::b::g;\n",
            "src/d/b.rs": "fn g() { super::a::f(); }\n",
        })
        self.assertEqual(got, {two_way("kernel:d::a", "kernel:d::b")})

    def test_root_alias(self) -> None:
        got = keys("mod d;\nmod e;\nuse d::a_init;\nuse e::b as bee;\n", {
            "src/d/mod.rs": "pub mod a_init;\n",
            "src/d/a_init.rs": "use crate::bee;\n",
            "src/e/mod.rs": "pub mod b;\n",
            "src/e/b.rs": "fn g() { crate::a_init::f(); }\n",
        })
        self.assertEqual(got, {two_way("kernel:d::a_init", "kernel:e::b")})

    def test_mod_rs_pub_use(self) -> None:
        got = keys("mod d;\nmod e;\n", {
            "src/d/mod.rs": "mod inner;\npub use inner::thing;\n",
            "src/d/inner/mod.rs": "pub mod thing;\n",
            "src/d/inner/thing.rs": "use crate::e::g;\n",
            "src/e.rs": "use crate::d::thing::X;\n",
        })
        self.assertEqual(got, {two_way("kernel:d::inner::thing", "kernel:e")})

    def test_core_alias_through_vibeos(self) -> None:
        got = keys("mod a;\n", {"src/a.rs": "use vibeos::thread::Tcb;\n"},
                   core_root="pub mod sched;\npub use sched::thread;\n")
        self.assertEqual(got, set())

    def test_one_way_passes(self) -> None:
        got = keys(self.ROOT_AB, {"src/a.rs": "use crate::b;\n", "src/b.rs": ""})
        self.assertEqual(got, set())

    def test_root_item_is_no_edge(self) -> None:
        got = keys("mod a;\n#[macro_export] macro_rules! m { () => {} }\n",
                   {"src/a.rs": "fn f() { crate::m!(); crate::A; }\n"})
        self.assertEqual(got, set())

    def test_comment_string_and_asm_ignored(self) -> None:
        got = keys(self.ROOT_AB, {
            "src/a.rs": "use crate::b;\n",
            "src/b.rs": "// crate::a::f\nconst S: &str = \"crate::a\";\n"
                        "fn g() { asm!(\"call {}\", \"crate::a::f\"); }\n",
        })
        self.assertEqual(got, set())

    def test_sym_operand_counts(self) -> None:
        got = keys(self.ROOT_AB, {
            "src/a.rs": "use crate::b;\n",
            "src/b.rs": "fn g() { asm!(\"call {f}\", f = sym crate::a::f); }\n",
        })
        self.assertEqual(got, {two_way("kernel:a", "kernel:b")})

    def test_cfg_test_and_kernel_tests_modules_excluded(self) -> None:
        got = keys("mod a;\nmod b;\n#[cfg(feature = \"kernel_tests\")]\nmod ktest;\n", {
            "src/a.rs": "use crate::b;\n#[cfg(test)]\nmod tests { use crate::b; }\n",
            "src/b.rs": "#[cfg(all(feature = \"kernel_tests\", target_os = \"none\"))]\n"
                        "pub mod testing { fn t() { crate::a::f(); } }\n"
                        "#[cfg(test)]\nmod tests;\n",
            "src/b/tests.rs": "use crate::a;\n",
            "src/ktest.rs": "use crate::a;\nuse crate::b;\n",
        })
        self.assertEqual(got, set())

    def test_not_kernel_tests_counts(self) -> None:
        got = keys(self.ROOT_AB, {
            "src/a.rs": "use crate::b;\n",
            "src/b.rs": "#[cfg(not(feature = \"kernel_tests\"))]\n"
                        "mod real { fn f() { crate::a::g(); } }\n",
        })
        self.assertEqual(got, {two_way("kernel:a", "kernel:b")})

    def test_gated_item_in_production_module_counts(self) -> None:
        got = keys(self.ROOT_AB, {
            "src/a.rs": "use crate::b;\n",
            "src/b.rs": "#[cfg(feature = \"kernel_tests\")]\nfn t() { crate::a::g(); }\n",
        })
        self.assertEqual(got, {two_way("kernel:a", "kernel:b")})

    def test_child_module_path(self) -> None:
        got = keys("mod d;\n", {
            "src/d/mod.rs": "mod a;\nfn f() { a::g(); }\n",
            "src/d/a.rs": "use super::f;\n",
        })
        self.assertEqual(got, {two_way("kernel:d", "kernel:d::a")})


class TestOneModule(unittest.TestCase):
    CORE = "pub mod fs;\npub mod other;\n"

    def test_siblings_skipped(self) -> None:
        got = keys("", {
            "crates/core/src/fs/mod.rs": "mod walk;\nmod file;\npub use walk::W;\n",
            "crates/core/src/fs/walk.rs": "use super::file::F;\n",
            "crates/core/src/fs/file.rs": "use super::walk::W;\nuse super::X;\n",
            "crates/core/src/other.rs": "",
        }, core_root=self.CORE)
        self.assertEqual(got, set())

    def test_outside_pair_not_skipped(self) -> None:
        got = keys("", {
            "crates/core/src/fs/mod.rs": "mod walk;\n",
            "crates/core/src/fs/walk.rs": "use crate::other::O;\n",
            "crates/core/src/other.rs": "use crate::fs::X;\n",
        }, core_root=self.CORE)
        self.assertEqual(got, {two_way("core:fs", "core:other")})


class TestHeapRule(unittest.TestCase):
    def test_heap_init_to_aliased_thread_init_fails(self) -> None:
        got = keys("mod mm;\nmod sched;\nmod sync;\nuse mm::heap_init;\n"
                   "use sched::thread_init;\nuse sync::sync_init;\n", {
                       "src/mm/mod.rs": "pub mod heap_init;\n",
                       "src/mm/heap_init.rs": "use crate::thread_init;\nuse crate::sync_init;\n",
                       "src/sched/mod.rs": "pub mod thread_init;\n",
                       "src/sched/thread_init.rs": "",
                       "src/sync/mod.rs": "pub mod sync_init;\n",
                       "src/sync/sync_init.rs": "",
                   })
        self.assertEqual(got, {"heap kernel:mm::heap_init -> kernel:sched::thread_init"})

    def test_core_pmm_to_fs_fails(self) -> None:
        got = keys("", {
            "crates/core/src/mm/mod.rs": "pub mod pmm;\n",
            "crates/core/src/mm/pmm.rs": "fn f() { crate::fs::x::g(); }\n",
            "crates/core/src/fs/mod.rs": "pub mod x;\n",
            "crates/core/src/fs/x.rs": "",
        }, core_root="pub mod mm;\npub mod fs;\n")
        self.assertEqual(got, {"heap core:mm::pmm -> core:fs"})

    def test_pmm_init_to_core_wait_fails(self) -> None:
        got = keys("mod mm;\n", {
            "src/mm/mod.rs": "pub mod pmm_init;\n",
            "src/mm/pmm_init.rs": "use vibeos::wait::WaitQueue;\n",
            "crates/core/src/sched/mod.rs": "pub mod wait;\n",
            "crates/core/src/sched/wait.rs": "",
        }, core_root="pub mod sched;\npub use sched::wait;\n")
        self.assertEqual(got, {"heap kernel:mm::pmm_init -> core:sched::wait"})


class TestRawRule(unittest.TestCase):
    def test_raw_to_x86_passes(self) -> None:
        self.assertEqual(keys("", {}), set())

    def test_raw_to_sync_init_fails(self) -> None:
        got = keys("mod sync;\nuse sync::sync_init;\n", {
            "src/sync/mod.rs": "pub mod sync_init;\n",
            "src/sync/sync_init.rs": "",
            "src/log/serial/raw.rs": "use crate::sync_init::SpinMutex;\n",
        })
        self.assertEqual(got, {"raw kernel:log::serial::raw -> kernel:sync::sync_init"})

    def test_raw_to_core_passes(self) -> None:
        got = keys("", {"src/log/serial/raw.rs": "use vibeos::uart;\n"},
                   core_root="pub mod uart;\n")
        self.assertEqual(got, set())

    def test_kernel_macro_in_raw_fails(self) -> None:
        got = keys("", {
            "src/log/serial/mod.rs": "pub mod raw;\n"
                                     "#[macro_export]\nmacro_rules! klog { () => {} }\n",
            "src/log/serial/raw.rs": "fn f() { crate::klog!(); }\n",
        })
        self.assertEqual(got, {"raw macro klog"})

    def test_missing_raw_fails(self) -> None:
        self.assertEqual(keys("mod a;\n", {"src/a.rs": ""}, raw=False), {"raw missing"})


class TestCheck(unittest.TestCase):
    def test_three_cycle_is_a_note(self) -> None:
        t = Tree("mod a;\nmod b;\nmod c;\n", {
            "src/a.rs": "use crate::b;\n", "src/b.rs": "use crate::c;\n",
            "src/c.rs": "use crate::a;\n"})
        try:
            errors, notes, _ = check(t.root, [])
        finally:
            t.close()
        self.assertEqual(errors, [])
        self.assertEqual(notes, ["cycle of 3: kernel:a kernel:b kernel:c"])

    def test_known_key_passes_and_stale_key_fails(self) -> None:
        t = Tree("mod a;\nmod b;\n", {"src/a.rs": "use crate::b;\n", "src/b.rs": "use crate::a;\n"})
        try:
            key = two_way("kernel:a", "kernel:b")
            errors, notes, _ = check(t.root, [key])
            self.assertEqual(errors, [])
            self.assertTrue(notes[0].startswith(f"known {key}: src/a.rs:1 ; src/b.rs:1"))
            errors, _, _ = check(t.root, [key, "raw missing"])
            self.assertEqual(errors, ["KNOWN: stale key 'raw missing'; delete it"])
        finally:
            t.close()

    def test_failure_prints_both_locations(self) -> None:
        t = Tree("mod a;\nmod b;\n", {"src/a.rs": "\nuse crate::b;\n",
                                      "src/b.rs": "\n\nuse crate::a::f;\n"})
        try:
            errors, _, _ = check(t.root, [])
        finally:
            t.close()
        self.assertEqual(errors, ["two-way kernel:a kernel:b: src/a.rs:2 ; src/b.rs:3"])


class TestTree(unittest.TestCase):
    def test_tree_nodes_include_raw_layer_parent(self) -> None:
        g, _, _, _ = build(ROOT)
        self.assertIn("kernel:log::serial", g.nodes)
        self.assertIn("core:sched::wait", g.nodes)
        self.assertNotIn("kernel:ktest", g.nodes)


if __name__ == "__main__":
    unittest.main()
