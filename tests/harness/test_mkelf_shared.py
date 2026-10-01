"""Host tests for scripts/mkelf_shared.py (ROADMAP §10.6, F031)."""

from __future__ import annotations

import contextlib
import io
import struct
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.mkelf_shared import (
    BASE,
    CONST,
    DATA,
    DATA_OFF,
    ENTRY_OFF,
    FILE_LEN,
    NAMES,
    PT_GNU_STACK,
    PT_LOAD,
    SHARED_OFF,
    build,
    main,
)

ROOT = Path(__file__).resolve().parent.parent.parent
FIXTURES = ROOT / "tests" / "fixtures" / "elf"


def phdrs(b: bytes) -> list[tuple[int, ...]]:
    phoff = struct.unpack_from("<Q", b, 32)[0]
    phnum = struct.unpack_from("<H", b, 56)[0]
    return [struct.unpack_from("<IIQQQQQQ", b, phoff + 56 * i) for i in range(phnum)]


class TestFixtures(unittest.TestCase):
    def test_checked_in_files_equal_build(self) -> None:
        for jump, name in NAMES.items():
            self.assertEqual((FIXTURES / name).read_bytes(), build(jump), name)
        self.assertEqual(main(["--check", str(FIXTURES)]), 0)

    def test_check_finds_a_difference(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.assertEqual(main(["--write", str(d)]), 0)
            self.assertEqual(main(["--check", str(d)]), 0)
            b = bytearray((d / NAMES[False]).read_bytes())
            b[DATA_OFF] ^= 1
            (d / NAMES[False]).write_bytes(bytes(b))
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(main(["--check", str(d)]), 1)


class TestLayout(unittest.TestCase):
    def test_layout(self) -> None:
        for jump in (False, True):
            b = build(jump)
            self.assertEqual(len(b), FILE_LEN)
            self.assertEqual(b[:4], b"\x7fELF")
            self.assertEqual(struct.unpack_from("<Q", b, 24)[0], BASE + ENTRY_OFF)
            rx, rw, stack = phdrs(b)
            # (type, flags, offset, vaddr, paddr, filesz, memsz, align)
            self.assertEqual(rx, (PT_LOAD, 5, 0, BASE, BASE, 0x1010, 0x1010, 0x1000))
            self.assertEqual(rw, (PT_LOAD, 6, 0x1010, BASE + 0x1010, BASE + 0x1010, 8, 8, 0x1000))
            self.assertEqual((stack[0], stack[1]), (PT_GNU_STACK, 6))
            # The two segments share the page at B + 0x1000, and each file
            # offset matches its address modulo the page.
            self.assertEqual((rx[3] + rx[6] - 1) & ~0xFFF, rw[3] & ~0xFFF)
            self.assertEqual(rw[2] % 0x1000, rw[3] % 0x1000)
            self.assertLess(BASE + ENTRY_OFF, BASE + SHARED_OFF)
            self.assertEqual(struct.unpack_from("<Q", b, DATA_OFF)[0], DATA)

    def test_constant_in_the_shared_page(self) -> None:
        b = build(False)
        self.assertEqual(struct.unpack_from("<Q", b, SHARED_OFF)[0], CONST)

    def test_jump_target(self) -> None:
        b = build(True)
        code = b[ENTRY_OFF : ENTRY_OFF + 12]
        # mov edi, 3; mov eax, B + 0x1000; jmp rax
        self.assertEqual(code[:5], b"\xbf\x03\x00\x00\x00")
        self.assertEqual(code[5], 0xB8)
        self.assertEqual(struct.unpack_from("<I", code, 6)[0], BASE + SHARED_OFF)
        self.assertEqual(code[10:12], b"\xff\xe0")
        # xor edi, edi; mov eax, 60; syscall; then int3 to the RW bytes.
        shared = b[SHARED_OFF:DATA_OFF]
        self.assertEqual(shared[:9], b"\x31\xff\xb8\x3c\x00\x00\x00\x0f\x05")
        self.assertEqual(set(shared[9:]), {0xCC})


class TestRunNative(unittest.TestCase):
    def test_refuses_other_hosts(self) -> None:
        for plat, machine in (("darwin", "arm64"), ("linux", "aarch64"), ("darwin", "x86_64")):
            with (
                mock.patch("sys.platform", plat),
                mock.patch("platform.machine", return_value=machine),
                contextlib.redirect_stderr(io.StringIO()) as err,
            ):
                self.assertEqual(main(["--run-native", str(FIXTURES)]), 2)
            self.assertIn("Linux x86_64", err.getvalue())


if __name__ == "__main__":
    unittest.main()
