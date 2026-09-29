"""Host tests for scripts/gen_ksyms.py (ROADMAP §10.2, F084)."""

from __future__ import annotations

import contextlib
import io
import os
import stat
import tempfile
import unittest
from pathlib import Path

from scripts import gen_ksyms
from scripts.gen_ksyms import check, parse_nm, render, rust_string, skip_name

NM_TEXT = """\
ffffffff80001000 T _start
ffffffff80001000 T start
ffffffff80001010 t zeta
ffffffff80001010 t alpha
ffffffff80001020 T vibeos::kmain
ffffffff80002000 r some_rodata
ffffffff80001030 W weak_fn
0000000000000000 T at_zero
ffffffff80001040 t .Ltmp1
ffffffff80001050 T __rustc::rust_begin_unwind
ffffffff80001060 T __rustc::other
"""


class ParseNmTest(unittest.TestCase):
    def test_sorted_by_address_then_name_one_per_address(self) -> None:
        self.assertEqual(
            parse_nm(NM_TEXT),
            [
                (0xFFFFFFFF80001000, "_start"),
                (0xFFFFFFFF80001010, "alpha"),
                (0xFFFFFFFF80001020, "vibeos::kmain"),
                (0xFFFFFFFF80001030, "weak_fn"),
                (0xFFFFFFFF80001050, "__rustc::rust_begin_unwind"),
            ],
        )

    def test_nm_order_at_one_address_does_not_matter(self) -> None:
        a = "ffffffff80001010 t zeta\nffffffff80001010 t alpha\n"
        b = "ffffffff80001010 t alpha\nffffffff80001010 t zeta\n"
        self.assertEqual(parse_nm(a), parse_nm(b))
        self.assertEqual(parse_nm(a), [(0xFFFFFFFF80001010, "alpha")])

    def test_thinlto_suffix_is_dropped(self) -> None:
        text = (
            "ffffffff80001000 t vibeos::dev::refill (.llvm.17683446639936173637)\n"
            "ffffffff80001010 t helper.llvm.7242210524149143754\n"
        )
        self.assertEqual(
            parse_nm(text),
            [(0xFFFFFFFF80001000, "vibeos::dev::refill"), (0xFFFFFFFF80001010, "helper")],
        )

    def test_long_names_are_cut(self) -> None:
        name = "x" * 200
        [(_, got)] = parse_nm(f"ffffffff80001000 T {name}\n")
        self.assertEqual(len(got), gen_ksyms.NAME_MAX)
        self.assertTrue(got.endswith("..."))

    def test_skip_name(self) -> None:
        for name in ("", ".text", "start", "$x", ".Lfoo", "__dso_handle", "anon.1", "__rustc::x"):
            self.assertTrue(skip_name(name), name)
        for name in ("kmain", "__rustc::rust_begin_unwind"):
            self.assertFalse(skip_name(name), name)


class RenderTest(unittest.TestCase):
    def test_empty_matches_build_rs_fallback(self) -> None:
        text = render([])
        self.assertIn('#[unsafe(link_section = ".ksyms")]', text)
        self.assertIn("#[used]", text)
        self.assertIn("static KSYMS: [vibeos::symtab::Entry; 0] = [\n];\n", text)
        build_rs = (Path(__file__).resolve().parents[2] / "build.rs").read_text(encoding="utf-8")
        self.assertIn(r'static KSYMS: [vibeos::symtab::Entry; 0] = [\n];\n', build_rs)

    def test_entries_in_a_sized_array_no_code_names(self) -> None:
        text = render([(0x10, "a"), (0x20, 'b"c')])
        self.assertIn("static KSYMS: [vibeos::symtab::Entry; 2] = [", text)
        self.assertNotIn("pub", text)
        self.assertIn('vibeos::symtab::Entry { addr: 0x10, name: "a" },', text)
        self.assertIn(r'vibeos::symtab::Entry { addr: 0x20, name: "b\"c" },', text)

    def test_rust_string(self) -> None:
        self.assertEqual(rust_string('a\\b"cé'), '"a\\\\b\\"c?"')


class CheckTest(unittest.TestCase):
    def test_same_table(self) -> None:
        t = render([(0x10, "a")])
        self.assertIsNone(check(t, t))

    def test_first_differing_line(self) -> None:
        old = render([(0x10, "a"), (0x20, "b")])
        new = render([(0x10, "a"), (0x30, "b")])
        diff = check(new, old)
        assert diff is not None
        self.assertIn("line 6", diff)
        self.assertIn("0x20", diff)
        self.assertIn("0x30", diff)

    def test_added_and_dropped_lines(self) -> None:
        self.assertIn("adds", check("a\nb\n", "a\n") or "")
        self.assertIn("drops", check("a\n", "a\nb\n") or "")


class MainTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.nm = self.dir / "nm"
        self.nm.write_text(f"#!/bin/sh\ncat <<'EOF'\n{NM_TEXT}EOF\n", encoding="utf-8")
        self.nm.chmod(self.nm.stat().st_mode | stat.S_IXUSR)
        self.elf = self.dir / "vibeos"
        self.elf.write_bytes(b"")
        self.table = self.dir / "k" / "ksyms.rs"

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def run_main(self, *args: str) -> tuple[int, str]:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            rc = gen_ksyms.main(["--nm", str(self.nm), *args, str(self.elf), str(self.table)])
        return rc, err.getvalue()

    def test_writes_then_checks(self) -> None:
        rc, _ = self.run_main()
        self.assertEqual(rc, 0)
        self.assertEqual(self.table.read_text(encoding="utf-8"), render(parse_nm(NM_TEXT)))
        rc, err = self.run_main("--check")
        self.assertEqual(rc, 0, err)

    def test_check_fails_on_a_different_table(self) -> None:
        self.table.parent.mkdir()
        self.table.write_text(render([(0xFFFFFFFF80001000, "_start")]), encoding="utf-8")
        rc, err = self.run_main("--check")
        self.assertEqual(rc, 1)
        self.assertIn(f"gen_ksyms: {self.elf}: table differs from the one it linked "
                      f"({self.table}): line ", err)

    def test_check_fails_without_a_table(self) -> None:
        rc, err = self.run_main("--check")
        self.assertEqual(rc, 1)
        self.assertIn(str(self.table), err)

    def test_nm_failure(self) -> None:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            rc = gen_ksyms.main(["--nm", os.path.join(self.tmp.name, "missing"),
                                 str(self.elf), str(self.table)])
        self.assertEqual(rc, 1)
        self.assertIn("failed", err.getvalue())


if __name__ == "__main__":
    unittest.main()
