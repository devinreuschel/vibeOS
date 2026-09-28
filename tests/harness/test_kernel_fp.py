"""Host tests for scripts/check_kernel_fp.py (ROADMAP §10.6, the FP binding box)."""

from __future__ import annotations

import contextlib
import io
import unittest
from unittest import mock

from scripts import check_kernel_fp
from scripts.check_kernel_fp import scan

HEAD = "vibeos:\tfile format elf64-x86-64\n\nDisassembly of section .text:\n\n"
H = "::h0123456789abcdef"


def listing(sym: str, *insns: str, section: str = ".text") -> list[str]:
    """A synthetic `llvm-objdump -d --no-show-raw-insn -M intel` listing."""
    head = HEAD.replace(".text", section)
    body = [f"ffffffff80001000 <{sym}>:"]
    body += [f"ffffffff8000{0x1000 + 4 * i:04x}:      \t{ins}" for i, ins in enumerate(insns)]
    return (head + "\n".join(body) + "\n").splitlines()


class TestScan(unittest.TestCase):
    def test_fp_instructions_outside_allow_list_fail(self) -> None:
        for ins in ("movaps\txmm0, xmm1", "fld\tst(0)", "fld\tqword ptr [rax]", "emms",
                    "vmovdqu\tymm0, ymmword ptr [rdi]", "ldmxcsr\tdword ptr [rsp]",
                    "movq\tmm0, rax", "movq\trax, xmm3", "vzeroupper"):
            with self.subTest(ins=ins):
                errs = scan(listing(f"vibeos::x::f{H}", "push\trbp", ins))
                self.assertEqual(len(errs), 1, errs)
                self.assertIn("vibeos::x::f: ", errs[0])

    def test_save_and_load_routines_pass(self) -> None:
        self.assertEqual(scan(listing(f"vibeos::proc::syscall_init::fp_save{H}",
                                      "fxsave64\t[rdi]", "ret")), [])
        self.assertEqual(scan(listing(f"vibeos::proc::syscall_init::fp_load{H}",
                                      "fxrstor64\t[rdi]", "ret")), [])
        self.assertEqual(scan(listing(f"vibeos::proc::syscall_init::fp_init_template{H}",
                                      "fninit", "fxsave64\t[rdi]", "ret")), [])

    def test_allow_list_is_per_mnemonic(self) -> None:
        errs = scan(listing(f"vibeos::proc::syscall_init::fp_save{H}", "fxrstor64\t[rdi]"))
        self.assertEqual(len(errs), 1, errs)
        errs = scan(listing(f"vibeos::proc::syscall_init::fp_load{H}", "movaps\txmm0, xmm1"))
        self.assertEqual(len(errs), 1, errs)

    def test_non_fp_instructions_pass(self) -> None:
        insns = ("mfence", "lfence", "sfence", "pause", "prefetcht0\tbyte ptr [rax]",
                 "prefetchw\tbyte ptr [rax]", "clflush\tbyte ptr [rax]",
                 "mov\trax, qword ptr [rsp + 0x8]", "lock\t\tcmpxchg\tqword ptr [rdi], rsi",
                 "rep\t\tmovsb\tbyte ptr es:[rdi], byte ptr [rsi]")
        self.assertEqual(scan(listing(f"vibeos::x::g{H}", *insns)), [])

    def test_hash_is_stripped_and_symbol_operands_ignored(self) -> None:
        errs = scan(listing(f"vibeos::x::h{H}", "movaps\txmm0, xmm1"))
        self.assertTrue(errs[0].startswith("vibeos::x::h: "), errs)
        call = "call\t0xffffffff80002000 <vibeos::xmm0::st(1)::mm0::fld::h0123456789abcdef>"
        self.assertEqual(scan(listing(f"vibeos::x::c{H}", call)), [])

    def test_llvm_suffix_is_stripped(self) -> None:
        sym = "vibeos::proc::syscall_init::fp_save (.llvm.11294618185531973296)"
        self.assertEqual(scan(listing(sym, "fxsave64\t[rdi]")), [])

    def test_trampoline_section_is_skipped(self) -> None:
        tramp = listing("vibeos_ap_trampoline", "fninit", "movaps\txmm0, xmm1",
                        section=".trampoline")
        self.assertEqual(scan(tramp), [])
        both = tramp + listing(f"vibeos::x::f{H}", "emms")
        self.assertEqual(len(scan(both)), 1)


class TestMain(unittest.TestCase):
    def test_no_elf_exits_zero(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_kernel_fp.main([]), 0)
        self.assertIn("check_kernel_fp: no ELF given", out.getvalue())

    def test_objdump_failure_exits_nonzero(self) -> None:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            rc = check_kernel_fp.main(["--objdump", "/nonexistent/llvm-objdump", "vibeos"])
        self.assertNotEqual(rc, 0)
        self.assertIn("objdump failed", err.getvalue())

    def test_listing_with_fp_fails(self) -> None:
        bad = listing(f"vibeos::x::f{H}", "movaps\txmm0, xmm1")
        with mock.patch.object(check_kernel_fp, "disassemble", return_value=bad), \
                contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(check_kernel_fp.main(["--objdump", "od", "k.elf"]), 1)

    def test_clean_listing_passes(self) -> None:
        good = listing(f"vibeos::x::f{H}", "mov\trax, rbx", "ret")
        out = io.StringIO()
        with mock.patch.object(check_kernel_fp, "disassemble", return_value=good), \
                contextlib.redirect_stdout(out):
            self.assertEqual(check_kernel_fp.main(["--objdump", "od", "k.elf"]), 0)
        self.assertIn("check_kernel_fp: ok", out.getvalue())


if __name__ == "__main__":
    unittest.main()
