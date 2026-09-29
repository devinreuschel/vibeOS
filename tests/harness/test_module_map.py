"""Host tests for scripts/check_module_map.py (DESIGN §1.3 equals the tree, A1)."""

from __future__ import annotations

import contextlib
import io
import unittest

from scripts import check_module_map
from scripts.check_module_map import Row, check, expand, parse_rows, section

DOC = """# Design

## 1.2 Other

| Subsystem | Portable | Kernel |
|---|---|---|
| decoy | `decoy.rs` | — |

## 1.3 Module map

Prose names `prose.rs` outside the table.

| Subsystem | Portable (`crates/core/src/`) | Kernel (`src/`) |
|---|---|---|
| crate | `lib.rs`, `kalloc.rs` | `main.rs` |
| mm | `mm/{mod,pmm}.rs` | `mm/{mod,pmm_init}.rs` (`BootInfo`) |
| fs | `fs/{mod,fat}.rs` | `fs/{mod,fat_init}.rs` |
| sched | `sched/mod.rs` | `sched/{mod,sched_init}.rs` |
| boot | — | `boot/mod.rs` |
| ktest | — | `ktest.rs`, `ktest/*.rs` |

## 1.4 Next

| Subsystem | Portable | Kernel |
|---|---|---|
| later | `later.rs` | — |
"""

CORE = {"lib.rs", "kalloc.rs", "mm/mod.rs", "mm/pmm.rs", "fs/mod.rs", "fs/fat.rs",
        "sched/mod.rs"}
KERNEL = {"main.rs", "mm/mod.rs", "mm/pmm_init.rs", "fs/mod.rs", "fs/fat_init.rs",
          "sched/mod.rs", "sched/sched_init.rs", "boot/mod.rs", "ktest.rs", "ktest/user.rs"}


def rows() -> list[Row]:
    return parse_rows(section(DOC))


def replace(table: list[Row], row: Row) -> list[Row]:
    return [row if r.subsystem == row.subsystem else r for r in table]


def run(core: set[str] = CORE, kernel: set[str] = KERNEL,
        table: list[Row] | None = None) -> list[str]:
    return check(rows() if table is None else table, set(core), set(kernel))


class TestParse(unittest.TestCase):
    def test_first_map_table_under_the_heading(self) -> None:
        got = rows()
        self.assertEqual([r.subsystem for r in got],
                         ["crate", "mm", "fs", "sched", "boot", "ktest"])
        self.assertEqual(got[1], Row("mm", ("mm/{mod,pmm}.rs",), ("mm/{mod,pmm_init}.rs",)))

    def test_prose_tokens_and_empty_cells(self) -> None:
        by = {r.subsystem: r for r in rows()}
        self.assertEqual(by["boot"].portable, ())
        self.assertEqual(by["mm"].kernel, ("mm/{mod,pmm_init}.rs",))

    def test_expand(self) -> None:
        self.assertEqual(expand("mm/{mod, pmm}.rs"), ["mm/mod.rs", "mm/pmm.rs"])
        self.assertEqual(expand("ktest/*.rs"), ["ktest/*.rs"])


class TestPasses(unittest.TestCase):
    def test_table_equal_to_tree(self) -> None:
        self.assertEqual(run(), [])

    def test_unlisted_subsystem_ktest_body(self) -> None:
        self.assertEqual(run(kernel=KERNEL | {"mm/ktest.rs"}), [])

    def test_unlisted_subsystem_ktest_child(self) -> None:
        kernel = KERNEL | {"mm/ktest.rs", "mm/ktest/heap.rs"}
        self.assertEqual(run(kernel=kernel), [])
        self.assertEqual(run(kernel=KERNEL | {"nope/ktest/heap.rs"}),
                         ["R4: kernel `nope/ktest/heap.rs` is in no row"])

    def test_flat_ktest_runner(self) -> None:
        self.assertIn("ktest.rs", check_module_map.FLAT_OK)
        self.assertEqual(run(), [])

    def test_r6_against_a_file_and_a_mod_dir(self) -> None:
        self.assertEqual(run(), [])
        table = replace(rows(), Row("fs", ("fs/mod.rs", "fs/fat/mod.rs"),
                                    ("fs/{mod,fat_init}.rs",)))
        core = (CORE - {"fs/fat.rs"}) | {"fs/fat/mod.rs"}
        self.assertEqual(run(core=core, table=table), [])

    def test_r6_sched_init_beside_sched_mod(self) -> None:
        errs = run()
        self.assertFalse([e for e in errs if "sched_init" in e])

    def test_root_only_module(self) -> None:
        kernel = KERNEL | {"mm/kalloc_init.rs"}
        table = replace(rows(), Row("mm", ("mm/{mod,pmm}.rs",),
                                    ("mm/{mod,pmm_init,kalloc_init}.rs",)))
        self.assertEqual(run(kernel=kernel, table=table), [])

    def test_main_on_this_repository(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_module_map.main(), 0)
        self.assertEqual(out.getvalue().strip(), "check_module_map: ok")


class TestFails(unittest.TestCase):
    def test_unlisted_core_file(self) -> None:
        self.assertEqual(run(core=CORE | {"mm/heap.rs"}),
                         ["R4: portable `mm/heap.rs` is in no row"])

    def test_unlisted_kernel_file(self) -> None:
        self.assertEqual(run(kernel=KERNEL | {"mm/heap_init.rs"}),
                         ["R4: kernel `mm/heap_init.rs` is in no row"])

    def test_listed_missing_file(self) -> None:
        self.assertEqual(run(core=CORE - {"mm/pmm.rs"}),
                         ["R2: portable `mm/pmm.rs` (row mm) does not exist"])

    def test_star_matching_nothing(self) -> None:
        self.assertEqual(run(kernel=KERNEL - {"ktest/user.rs"}),
                         ["R2: kernel `ktest/*.rs` (row ktest) matches no file"])

    def test_listed_twice(self) -> None:
        table = rows() + [Row("fs2", (), ("fs/mod.rs",))]
        errs = run(table=table)
        self.assertIn("R3: kernel `fs/mod.rs` listed twice (rows fs, fs2)", errs)

    def test_duplicate_row(self) -> None:
        errs = run(table=rows() + [Row("boot", (), ())])
        self.assertEqual(errs, ["R1: row boot appears 2 times"])

    def test_path_outside_its_row(self) -> None:
        table = replace(replace(rows(), Row("boot", (), ("boot/mod.rs", "main.rs"))),
                        Row("crate", ("lib.rs", "kalloc.rs"), ()))
        errs = run(table=table)
        self.assertEqual(errs, ["R5: kernel `main.rs` lies outside row boot's directory"])

    def test_r6_init_apart_from_its_portable_half(self) -> None:
        kernel = (KERNEL - {"fs/fat_init.rs"}) | {"mm/fat_init.rs"}
        table = replace(replace(rows(), Row("mm", ("mm/{mod,pmm}.rs",),
                                            ("mm/{mod,pmm_init,fat_init}.rs",))),
                        Row("fs", ("fs/{mod,fat}.rs",), ("fs/mod.rs",)))
        errs = run(kernel=kernel, table=table)
        self.assertEqual(errs, ["R6: kernel `mm/fat_init.rs` sits apart from portable `fat` "
                                "(allowed: `fs`)"])

if __name__ == "__main__":
    unittest.main()
