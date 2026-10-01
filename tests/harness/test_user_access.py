"""Host tests for scripts/check_user_access.py (ROADMAP §10.6: one named fill
API for writing an address space that is not running, and `stac` only in the
accessor module)."""

from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import check_user_access as cua

FILL, FILL_INIT = cua.FILL_MODULE
OTHER = "src/proc/user_init.rs"
FILL_SRC = "".join(f"unsafe fn {n}() {{}}\n" for n in cua.PRIMITIVES)


def tree(files: dict[str, str]) -> tempfile.TemporaryDirectory[str]:
    """A temporary root holding the fill module pair, the accessor module and
    `files`, which override them."""
    d = tempfile.TemporaryDirectory()
    root = Path(d.name)
    base = {FILL: FILL_SRC, FILL_INIT: "", cua.ACCESSOR_MODULE: 'asm!("stac", "clac");\n'}
    for rel, text in {**base, **files}.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")
    return d


def run(files: dict[str, str]) -> list[str]:
    with tree(files) as d:
        return cua.check(Path(d))


class RuleA(unittest.TestCase):
    def test_primitive_outside_the_fill_api_fails(self) -> None:
        for name in cua.PRIMITIVES:
            with self.subTest(name=name):
                errs = run({OTHER: f"fn f() {{\n    unsafe {{ {name}(0, 0, 0, &[]) }};\n}}\n"})
                self.assertEqual(errs, [f"{OTHER}:2: {name} outside the fill API (ROADMAP §10.6)"])

    def test_crates_are_in_scope(self) -> None:
        errs = run({"crates/core/src/proc/addr_space/mod.rs": "use super::fill::physmap_zero;\n"})
        self.assertEqual(len(errs), 1)

    def test_comments_strings_and_longer_names_pass(self) -> None:
        text = (
            "// physmap_write is the fill API's\n"
            "/* physmap_zero\n   /* nested physmap_copy */ */\n"
            'const S: &str = "physmap_copy";\n'
            'const R: &str = r#"physmap_write "quoted""#;\n'
            "fn physmap_write_count() {}\n"
            "fn f<'a>(x: &'a u8) -> char { 'p' }\n"
        )
        self.assertEqual(run({OTHER: text}), [])

    def test_the_fill_api_may_call_them(self) -> None:
        body = FILL_SRC + "fn w() { unsafe { physmap_write() } }\n"
        self.assertEqual(run({FILL: body, FILL_INIT: "// calls through fill\n"}), [])


class RuleB(unittest.TestCase):
    def test_stac_call_outside_the_accessor_module_fails(self) -> None:
        errs = run({"src/proc/ktest/entry.rs": "fn f() {\n    x86::stac();\n}\n"})
        self.assertEqual(
            errs, ["src/proc/ktest/entry.rs:2: stac outside the accessor module (ROADMAP §10.6)"])

    def test_stac_in_an_asm_template_fails(self) -> None:
        text = 'fn f() { unsafe { asm!("stac", options(nostack)) } }\n'
        errs = run({"src/arch/x86_64/cpu.rs": text})
        self.assertEqual(len(errs), 1)
        self.assertIn("stac outside the accessor module", errs[0])

    def test_stac_in_assembly_fails(self) -> None:
        path = "src/arch/x86_64/trampoline.S"
        errs = run({path: "# stac in a comment\n    stac\n"})
        self.assertEqual(errs, [f"{path}:2: stac outside the accessor module (ROADMAP §10.6)"])

    def test_comments_and_other_words_pass(self) -> None:
        text = (
            "// lifts SMAP with stac\n"
            "/// `stac`/`clac` are the accessor module's\n"
            "fn f(stack: u64) -> u64 { stack + STACK_TOP }\n"
        )
        self.assertEqual(run({OTHER: text}), [])

    def test_the_accessor_module_may_use_it(self) -> None:
        self.assertEqual(run({cua.ACCESSOR_MODULE: "pub fn stac() { asm!(\"stac\") }\n"}), [])


class RuleC(unittest.TestCase):
    def test_listed_name_missing_from_fill_fails(self) -> None:
        missing = cua.PRIMITIVES[0]
        body = "".join(f"unsafe fn {n}() {{}}\n" for n in cua.PRIMITIVES[1:])
        errs = run({FILL: body})
        self.assertEqual(len(errs), 1)
        self.assertIn(f"listed primitive {missing} has no fn here", errs[0])

    def test_unlisted_physmap_fn_fails(self) -> None:
        errs = run({FILL: FILL_SRC + "unsafe fn physmap_extra() {}\n"})
        self.assertEqual(errs, [f"{FILL}: fn physmap_extra is not in PRIMITIVES (ROADMAP §10.6)"])

    def test_missing_module_file_fails(self) -> None:
        with tree({}) as d:
            (Path(d) / FILL_INIT).unlink()
            errs = cua.check(Path(d))
        self.assertEqual(errs, [f"{FILL_INIT}: the fill API module is missing (ROADMAP §10.6)"])


class Main(unittest.TestCase):
    def test_ok_line(self) -> None:
        out = io.StringIO()
        with tree({}) as d, contextlib.redirect_stdout(out):
            rc = cua.main(["--root", d])
        self.assertEqual((rc, out.getvalue()), (0, "check_user_access: ok\n"))

    def test_failure_exits_1(self) -> None:
        err = io.StringIO()
        with tree({OTHER: "fn f() { x86::stac() }\n"}) as d, contextlib.redirect_stderr(err):
            rc = cua.main(["--root", d])
        self.assertEqual(rc, 1)
        self.assertIn("stac outside the accessor module", err.getvalue())


if __name__ == "__main__":
    unittest.main()
