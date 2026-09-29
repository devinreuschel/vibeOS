"""Host tests for scripts/check_user_elf.py (ROADMAP §10.5, the user triple box)."""

from __future__ import annotations

import contextlib
import io
import struct
import unittest

from scripts import check_user_elf
from scripts.check_user_elf import PF_W, PF_X, PT_DYNAMIC, PT_INTERP, PT_LOAD, check_elf

PF_R = 4
BASE = 0x4000_0000

# (p_type, p_flags, p_offset, p_vaddr, p_filesz, p_memsz)
Phdr = tuple[int, int, int, int, int, int]

GOOD: list[Phdr] = [
    (PT_LOAD, PF_R, 0x0, BASE, 0x200, 0x200),
    (PT_LOAD, PF_R | PF_X, 0x1000, BASE + 0x1000, 0x800, 0x800),
    (PT_LOAD, PF_R | PF_W, 0x2000, BASE + 0x2000, 0x10, 0x3000),
]


def elf(
    phdrs: list[Phdr] | None = None,
    syms: list[str] | None = None,
    *,
    e_type: int = 2,
    symtab: bool = True,
) -> bytes:
    """A minimal ELF64 image: ehdr, phdrs, and optionally `.symtab`/`.strtab`
    with their section headers. Segment bytes are zero-filled to the end of
    the furthest segment. A symbol named `U:<name>` is undefined."""
    phdrs = GOOD if phdrs is None else phdrs
    syms = ["_start"] if syms is None else syms
    phoff = 64
    body_end = max([phoff + 56 * len(phdrs)] + [p[2] + p[4] for p in phdrs])
    names = [s.removeprefix("U:") for s in syms]
    strtab = b"\0" + b"".join(n.encode() + b"\0" for n in names)
    symtab_b = b"\0" * 24
    off = 1
    for s, n in zip(syms, names, strict=True):
        shndx, value = (0, 0) if s.startswith("U:") else (1, BASE + 0x1000)
        symtab_b += struct.pack("<IBBHQQ", off, 0x12, 0, shndx, value, 0)
        off += len(n) + 1
    sym_off = body_end
    str_off = sym_off + len(symtab_b)
    shoff = str_off + len(strtab)
    shdrs = b"\0" * 64
    if symtab:
        shdrs += struct.pack("<IIQQQQIIQQ", 0, 2, 0, 0, sym_off, len(symtab_b), 2, 1, 8, 24)
        shdrs += struct.pack("<IIQQQQIIQQ", 0, 3, 0, 0, str_off, len(strtab), 0, 0, 1, 0)
    shnum = len(shdrs) // 64
    ehdr = b"\x7fELF" + bytes([2, 1, 1]) + b"\0" * 9
    ehdr += struct.pack(
        "<HHIQQQIHHHHHH", e_type, 62, 1, BASE + 0x1000, phoff, shoff, 0, 64, 56, len(phdrs),
        64, shnum, 0,
    )
    ph = b"".join(
        struct.pack("<IIQQQQQQ", t, f, o, v, v, fs, ms, 0x1000) for (t, f, o, v, fs, ms) in phdrs
    )
    img = bytearray(ehdr + ph)
    img += b"\0" * (body_end - len(img))
    return bytes(img) + symtab_b + strtab + shdrs


class TestCheckElf(unittest.TestCase):
    def assert_fails(self, data: bytes, needle: str) -> None:
        errs = check_elf(data)
        self.assertTrue(any(needle in e for e in errs), errs)

    def test_good_image_passes(self) -> None:
        self.assertEqual(check_elf(elf()), [])

    def test_et_dyn_fails(self) -> None:
        self.assert_fails(elf(e_type=3), "ET_EXEC")

    def test_load_past_2gib_fails(self) -> None:
        self.assert_fails(
            elf([(PT_LOAD, PF_R, 0x0, 0x7FFF_F000, 0x100, 0x2000)]), "past 2 GiB"
        )

    def test_load_ending_at_2gib_passes(self) -> None:
        self.assertEqual(check_elf(elf([(PT_LOAD, PF_R, 0x0, 0x7FFF_F000, 0x100, 0x1000)])), [])

    def test_interp_fails(self) -> None:
        self.assert_fails(elf(GOOD + [(PT_INTERP, PF_R, 0x100, BASE + 0x100, 0x10, 0x10)]),
                          "PT_INTERP")

    def test_dynamic_fails(self) -> None:
        self.assert_fails(elf(GOOD + [(PT_DYNAMIC, PF_R, 0x100, BASE + 0x100, 0x10, 0x10)]),
                          "PT_DYNAMIC")

    def test_shared_memory_page_fails(self) -> None:
        # File pages differ; the second segment's memory starts on the
        # first one's last page.
        self.assert_fails(
            elf([
                (PT_LOAD, PF_R, 0x0, BASE, 0x100, 0x1100),
                (PT_LOAD, PF_R | PF_X, 0x2000, BASE + 0x1800, 0x100, 0x100),
            ]),
            "share a page in memory",
        )

    def test_shared_file_page_fails(self) -> None:
        # Memory pages differ; both segments' bytes sit on file page 0.
        self.assert_fails(
            elf([
                (PT_LOAD, PF_R, 0x0, BASE, 0x100, 0x100),
                (PT_LOAD, PF_R | PF_X, 0x800, BASE + 0x1800, 0x100, 0x100),
            ]),
            "share a page in the file",
        )

    def test_bss_only_segment_shares_no_file_page(self) -> None:
        self.assertEqual(
            check_elf(elf([
                (PT_LOAD, PF_R, 0x0, BASE, 0x100, 0x100),
                (PT_LOAD, PF_R | PF_W, 0x100, BASE + 0x1000, 0x0, 0x1000),
            ])),
            [],
        )

    def test_write_and_execute_fails(self) -> None:
        self.assert_fails(elf([(PT_LOAD, PF_R | PF_W | PF_X, 0x0, BASE, 0x100, 0x100)]),
                          "writable and executable")

    def test_no_symtab_fails(self) -> None:
        self.assert_fails(elf(symtab=False), "run before strip")

    def test_defined_soft_float_fails(self) -> None:
        self.assert_fails(elf(syms=["_start", "__muldf3"]), "__muldf3")

    def test_undefined_soft_float_fails(self) -> None:
        self.assert_fails(elf(syms=["_start", "U:__addsf3"]), "__addsf3")

    def test_ti_mode_and_powi_pass(self) -> None:
        self.assertEqual(check_elf(elf(syms=["_start", "__floattidf", "__powidf2"])), [])

    def test_truncated_fails_without_traceback(self) -> None:
        full = elf()
        for n in (0, 10, 63, 64, 100, 64 + 56 * 3 - 1, len(full) - 1):
            with self.subTest(n=n):
                self.assertNotEqual(check_elf(full[:n]), [])

    def test_not_elf64_le_fails(self) -> None:
        data = bytearray(elf())
        data[4] = 1
        self.assert_fails(bytes(data), "ELF64")

    def test_soft_float_set(self) -> None:
        for name in ("__adddf3", "__subsf3", "__negdf2", "__extendsfdf2", "__truncdfsf2",
                     "__fixsfsi", "__fixunsdfdi", "__floatsisf", "__floatundidf", "__unordsf2",
                     "__gtdf2"):
            self.assertIn(name, check_user_elf.SOFT_FLOAT)
        for name in ("__floattidf", "__fixdfti", "__powidf2", "__powisf2", "memcpy"):
            self.assertNotIn(name, check_user_elf.SOFT_FLOAT)


class TestMain(unittest.TestCase):
    def test_no_path_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_user_elf.main([]), 0)
        self.assertIn("no ELF given; nothing to check", out.getvalue())


if __name__ == "__main__":
    unittest.main()
