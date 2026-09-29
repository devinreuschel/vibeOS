"""Host tests for scripts/check_test_hooks.py (Q2's `nm` check, ROADMAP §10.2, F146)."""

from __future__ import annotations

import contextlib
import io
import os
import stat
import tempfile
import unittest
from pathlib import Path

from scripts import check_test_hooks
from scripts.check_test_hooks import scan, symbol_name

H = "::h0123456789abcdef"


def nm_line(name: str, kind: str = "t", addr: str | None = "ffffffff80001000") -> str:
    """One `llvm-nm --demangle` line: `addr type name`, or `type name` when undefined."""
    return f"{addr} {kind} {name}" if addr is not None else f"{kind} {name}"


class TestSymbolName(unittest.TestCase):
    def test_three_and_two_field_lines(self) -> None:
        self.assertEqual(symbol_name(nm_line(f"vibeos::a::b{H}")), "vibeos::a::b")
        self.assertEqual(symbol_name(nm_line("vibeos_setjmp", "U", None)), "vibeos_setjmp")

    def test_names_with_spaces_keep_them(self) -> None:
        name = "<vibeos::x::Y as core::fmt::Debug>::fmt"
        self.assertEqual(symbol_name(nm_line(name + H)), name)

    def test_llvm_suffix_and_junk(self) -> None:
        self.assertEqual(symbol_name(nm_line("vibeos::a::f.llvm.12345")), "vibeos::a::f")
        self.assertIsNone(symbol_name(""))
        self.assertIsNone(symbol_name("vibeos:"))


class TestScan(unittest.TestCase):
    def test_catch_module_fails(self) -> None:
        for name in ("vibeos::arch::x86_64::catch::intercept", "vibeos::arch::catch::LAST",
                     "vibeos::arch::x86_64::catch::catch_skip::<F>"):
            with self.subTest(name=name):
                errs = scan([nm_line(name + H)])
                self.assertEqual(len(errs), 1, errs)
                self.assertIn("arch::catch", errs[0])

    def test_catch_asm_symbols_fail_whole_name_only(self) -> None:
        for name in ("vibeos_jmpbuf", "vibeos_setjmp", "vibeos_longjmp", "vibeos_catch",
                     "vibeos_catch_thunk"):
            with self.subTest(name=name):
                self.assertEqual(len(scan([nm_line(name, "T")])), 1)
        for name in ("vibeos_setjmp_user", "vibeos_catchall", "vibeos::x::vibeos_setjmp_count"):
            with self.subTest(name=name):
                self.assertEqual(scan([nm_line(name, "T")]), [])

    def test_fail_next_last_segment(self) -> None:
        self.assertEqual(len(scan([nm_line("vibeos::block::block_init::FAIL_NEXT", "b")])), 1)
        self.assertEqual(len(scan([nm_line("FAIL_NEXT", "b")])), 1)
        self.assertEqual(scan([nm_line("vibeos::block::block_init::FAIL_NEXT_X", "b")]), [])
        self.assertEqual(scan([nm_line("vibeos::block::FAIL_NEXT::f" + H)]), [])

    def test_q2_mutation_hooks(self) -> None:
        for name in ("vibeos::console::kbd_init::push_for_test",
                     "vibeos::smp::smp_init::exercise_fail_cleanup",
                     "vibeos::block::block_init::inject_io_fails"):
            with self.subTest(name=name):
                self.assertEqual(len(scan([nm_line(name + H)])), 1)

    def test_production_names_pass(self) -> None:
        names = ("vibeos::arch::x86_64::idt::trap_dispatch", "vibeos::block::block_init::execute",
                 "vibeos::proc::catch_signal", "vibeos::log::panic::panic",
                 "vibeos::arch::x86_64::catchall::f")
        self.assertEqual(scan([nm_line(n + H) for n in names]), [])


class TestMain(unittest.TestCase):
    def run_main(self, argv: list[str]) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = check_test_hooks.main(argv)
        return rc, out.getvalue(), err.getvalue()

    def fake_nm(self, d: Path, lines: list[str]) -> str:
        """An executable that prints `lines` whatever its arguments."""
        listing = d / "listing.txt"
        listing.write_text("".join(ln + "\n" for ln in lines))
        nm = d / "nm"
        nm.write_text(f"#!/bin/sh\ncat '{listing}'\n")
        nm.chmod(nm.stat().st_mode | stat.S_IXUSR)
        return str(nm)

    def test_clean_elf_exits_0(self) -> None:
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            (d / "vibeos").write_bytes(b"\x7fELF")
            nm = self.fake_nm(d, [nm_line("vibeos::main::_start" + H, "T")])
            rc, out, _ = self.run_main(["--elf", str(d / "vibeos"), "--nm", nm])
            self.assertEqual(rc, 0)
            self.assertIn("ok", out)

    def test_hook_symbol_exits_1(self) -> None:
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            (d / "vibeos").write_bytes(b"\x7fELF")
            nm = self.fake_nm(d, [nm_line("vibeos_catch", "T"),
                                  nm_line("vibeos::block::block_init::FAIL_NEXT", "b")])
            rc, _, err = self.run_main(["--elf", str(d / "vibeos"), "--nm", nm])
            self.assertEqual(rc, 1)
            self.assertIn("vibeos_catch:", err)
            self.assertIn("FAIL_NEXT:", err)

    def test_missing_elf_or_nm_exits_2(self) -> None:
        with tempfile.TemporaryDirectory() as t:
            d = Path(t)
            rc, _, err = self.run_main(["--elf", str(d / "none"), "--nm", "true"])
            self.assertEqual(rc, 2)
            self.assertIn("no such ELF", err)
            (d / "vibeos").write_bytes(b"\x7fELF")
            rc, _, _ = self.run_main(["--elf", str(d / "vibeos"), "--nm", str(d / "no-nm")])
            self.assertEqual(rc, 2)

    def test_default_elf_follows_cargo_target_dir(self) -> None:
        old = os.environ.get("CARGO_TARGET_DIR")
        try:
            os.environ["CARGO_TARGET_DIR"] = "/x/t"
            self.assertEqual(check_test_hooks.default_elf(),
                             "/x/t/x86_64-unknown-none/hookcheck/vibeos")
            del os.environ["CARGO_TARGET_DIR"]
            self.assertEqual(check_test_hooks.default_elf(),
                             "target/x86_64-unknown-none/hookcheck/vibeos")
        finally:
            if old is None:
                os.environ.pop("CARGO_TARGET_DIR", None)
            else:
                os.environ["CARGO_TARGET_DIR"] = old


if __name__ == "__main__":
    unittest.main()
