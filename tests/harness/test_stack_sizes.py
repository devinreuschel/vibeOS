"""Host tests for scripts/check_stack_sizes.py (ROADMAP §10.2, F058)."""

from __future__ import annotations

import contextlib
import io
import re
import struct
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import check_stack_sizes as css

ROOT = Path(__file__).resolve().parents[2]


def entry(addr: int, size: int) -> bytes:
    """One `.stack_sizes` entry: the address and the ULEB128 size."""
    out = bytearray(struct.pack("<Q", addr))
    while True:
        b = size & 0x7F
        size >>= 7
        out.append(b | (0x80 if size else 0))
        if not size:
            return bytes(out)


A, B, C, D, E = (0xFFFFFFFF80001000 + 0x100 * i for i in range(5))

# A synthetic `llvm-objdump -d --no-show-raw-insn -C` listing: the entry
# calls a, which tail-jumps to b; c's address is loaded by code (lea) and
# d's as a negative immediate; e is never named.
LISTING = f"""
vibeos:\tfile format elf64-x86-64

Disassembly of section .text:

{A:016x} <vibeos_syscall_entry>:
{A:x}:      \tcallq\t0x{B:x} <a>
{A + 5:x}:      \tleaq\t0x1234(%rip), %rax      # 0x{D:x} <c>
{A + 12:x}:      \tretq

{B:016x} <a>:
{B:x}:      \tjne\t0x{B + 8:x} <a+0x8>
{B + 2:x}:      \tjmp\t0x{C:x} <b>
{B + 8:x}:      \tcallq\t*%rax

{C:016x} <b>:
{C:x}:      \tretq

{D:016x} <c>:
{D:x}:      \tmovq\t$-0x{(1 << 64) - E:x}, %rax
{D + 7:x}:      \tretq

{E:016x} <d>:
{E:x}:      \tretq
"""


class TestParse(unittest.TestCase):
    def test_uleb128(self) -> None:
        self.assertEqual(css.uleb128(b"\x08", 0), (8, 1))
        self.assertEqual(css.uleb128(b"\xe5\x8e\x26", 0), (624485, 3))
        with self.assertRaises(ValueError):
            css.uleb128(b"\x80", 0)

    def test_parse_stack_sizes(self) -> None:
        data = entry(A, 8) + entry(B, 5000) + entry(B, 16) + entry(0, 99)
        self.assertEqual(css.parse_stack_sizes(data), {A: 8, B: 5000, 0: 99})
        with self.assertRaisesRegex(ValueError, "truncated"):
            css.parse_stack_sizes(entry(A, 8)[:5])

    def test_parse_objdump(self) -> None:
        d = css.parse_objdump(LISTING)
        self.assertEqual(d.names[A], "vibeos_syscall_entry")
        self.assertEqual(d.edges[A], {B})
        # a's jump into itself is no edge; its tail jump to b is.
        self.assertEqual(d.edges[B], {C})
        self.assertEqual(d.code_refs, {D, E})

    def test_address_taken_in_data(self) -> None:
        names = {A: "x", B: "y", C: "z"}
        rodata = css.Section(".rodata", css.SHF_ALLOC, 1, 0x1000,
                             b"\0" * 4 + struct.pack("<Q", C) + struct.pack("<Q", B)[:4]
                             + struct.pack("<Q", B))
        # Unaligned: C at offset 4 is not taken; B at offset 16 is.
        text = css.Section(".text", css.SHF_ALLOC | css.SHF_EXECINSTR, 1, 0x2000,
                           struct.pack("<Q", A))
        info = css.Section(".stack_sizes", 0, 1, 0, struct.pack("<Q", A))
        self.assertEqual(css.address_taken(names, [], [rodata, text, info]), {B})
        self.assertEqual(css.address_taken(names, [A, 0x42], []), {A})


class TestLinkerBounds(unittest.TestCase):
    def test_linker_bounds(self) -> None:
        script = "SECTIONS {\n    __text_start = .;\n  .text : { *(.text) }\n  x = 4;\n}\n"
        syms = {"__text_start": A, "x": 4, "vibeos_syscall_entry": B}
        self.assertEqual(css.linker_bounds(script, syms), {A, 4})

    def test_base_name(self) -> None:
        self.assertEqual(css.base_name("f (.llvm.123)"), "f")
        self.assertEqual(css.base_name("g"), "g")


class TestReach(unittest.TestCase):
    def test_reachable_and_chain(self) -> None:
        edges = {1: {2}, 2: {3}, 3: set(), 4: {5}, 5: set(), 6: {1}}
        parent = css.reachable([1, 4], edges)
        self.assertEqual(set(parent), {1, 2, 3, 4, 5})
        self.assertEqual(css.chain(parent, 3), [1, 2, 3])

    def test_violations(self) -> None:
        parent = css.reachable([1], {1: {2, 3}, 2: set(), 3: set()})
        sizes = {1: 100, 2: 5000, 3: 4096, 9: 99999}
        v = css.violations(sizes, parent, 4096)
        self.assertEqual([(x.addr, x.frame, x.chain) for x in v], [(2, 5000, (1, 2))])


def elf(sections: list[tuple[str, int, int, bytes]]) -> bytes:
    """A minimal ELF64 file holding `sections` (name, flags, addr, data)."""
    names = b"\0" + b"".join(n.encode() + b"\0" for n, _, _, _ in sections) + b".shstrtab\0"
    body = bytearray(b"\0" * 64)
    offs = []
    for _, _, _, data in sections:
        offs.append(len(body))
        body += data
    stroff = len(body)
    body += names
    shoff = len(body)
    shdrs = [bytes(64)]
    at = 1
    for (n, flags, addr, data), off in zip(sections, offs, strict=True):
        shdrs.append(struct.pack("<IIQQQQIIQQ", at, 1, flags, addr, off, len(data), 0, 0, 1, 0))
        at += len(n) + 1
    shdrs.append(struct.pack("<IIQQQQIIQQ", at, 3, 0, 0, stroff, len(names), 0, 0, 1, 0))
    body += b"".join(shdrs)
    hdr = bytearray(64)
    hdr[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<Q", hdr, 0x28, shoff)
    struct.pack_into("<HHH", hdr, 0x3A, 64, len(shdrs), len(shdrs) - 1)
    body[:64] = hdr
    return bytes(body)


class TestMain(unittest.TestCase):
    known: dict[str, tuple[int, str]] = {}
    not_roots: tuple[str, ...] = ()

    def run_main(self, blob: bytes | None, listing: str = LISTING,
                 *extra: str) -> tuple[int, str, str]:
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "vibeos.elf"
            if blob is not None:
                path.write_bytes(blob)
            fake = Path(d) / "objdump"
            fake.write_text("#!/bin/sh\ncat <<'EOF'\n" + listing + "\nEOF\n")
            fake.chmod(0o755)
            out, err = io.StringIO(), io.StringIO()
            with (
                contextlib.redirect_stdout(out),
                contextlib.redirect_stderr(err),
                mock.patch.object(css, "BOUND_BYTES", 4096),
                mock.patch.object(css, "KNOWN_OVER", self.known),
                mock.patch.object(css, "NOT_ROOTS", self.not_roots),
            ):
                rc = css.main(["--elf", str(path), "--objdump", str(fake), *extra])
        return rc, out.getvalue(), err.getvalue()

    def sizes(self, **frames: int) -> bytes:
        addr = {"entry": A, "a": B, "b": C, "c": D, "d": E}
        return b"".join(entry(addr[k], v) for k, v in frames.items())

    def test_ok(self) -> None:
        blob = elf([(".stack_sizes", 0, 0, self.sizes(entry=8, a=4096, b=64, c=100))])
        rc, out, _ = self.run_main(blob)
        self.assertEqual(rc, 0)
        self.assertIn("check_stack_sizes: ok (5 reachable functions, largest 4096 a;", out)

    def test_frame_over_bound_names_chain(self) -> None:
        blob = elf([(".stack_sizes", 0, 0, self.sizes(entry=8, a=16, b=9000))])
        rc, _, err = self.run_main(blob)
        self.assertEqual(rc, 1)
        self.assertIn("b: frame 9000 bytes over 4096; reached by "
                      "vibeos_syscall_entry -> a -> b", err)

    def test_address_taken_is_a_root(self) -> None:
        # d is reached only through c's immediate; e's frame counts.
        blob = elf([(".stack_sizes", 0, 0, self.sizes(d=5000))])
        rc, _, err = self.run_main(blob)
        self.assertEqual(rc, 1)
        self.assertIn("d: frame 5000 bytes", err)

    def test_unreached_and_discarded_ignored(self) -> None:
        listing = LISTING + f"\n{E + 0x100:016x} <boot_only>:\n{E + 0x100:x}:      \tretq\n"
        blob = elf([(".stack_sizes", 0, 0,
                     self.sizes(entry=8) + entry(E + 0x100, 9000) + entry(0, 9000))])
        rc, _, _ = self.run_main(blob, listing)
        self.assertEqual(rc, 0)

    def test_known_over(self) -> None:
        blob = elf([(".stack_sizes", 0, 0, self.sizes(entry=8, b=9000))])
        self.known = {"b": (9000, "why")}
        rc, out, _ = self.run_main(blob)
        self.assertEqual(rc, 0)
        self.assertIn("1 known over it", out)
        # Grown past its recorded frame.
        self.known = {"b": (8999, "why")}
        rc, _, err = self.run_main(blob)
        self.assertEqual(rc, 1)
        self.assertIn("b: frame 9000 bytes over 4096", err)
        self.assertNotIn("delete it", err)
        # Under the bound again: the entry is stale.
        self.known = {"b": (9000, "why")}
        small = elf([(".stack_sizes", 0, 0, self.sizes(entry=8, b=100))])
        rc, _, err = self.run_main(small)
        self.assertEqual(rc, 1)
        self.assertIn("KNOWN_OVER entry 'b' is not over the bound: delete it", err)
        self.known = {}

    def test_not_roots(self) -> None:
        # d is reached only as an address-taken root; NOT_ROOTS drops it.
        blob = elf([(".stack_sizes", 0, 0, self.sizes(entry=8, d=9000))])
        self.not_roots = ("d",)
        rc, out, _ = self.run_main(blob)
        self.assertEqual(rc, 0, out)
        self.not_roots = ("gone",)
        rc, _, err = self.run_main(blob)
        self.assertEqual(rc, 1)
        self.assertIn("no function gone (NOT_ROOTS)", err)
        self.not_roots = ()

    def test_report_lists_no_entry(self) -> None:
        blob = elf([(".stack_sizes", 0, 0, self.sizes(entry=8, a=300))])
        rc, out, _ = self.run_main(blob, LISTING, "--report", "1")
        self.assertEqual(rc, 0)
        self.assertIn("1 largest reachable frames:\n     300 a\n", out)
        self.assertIn("reachable functions with no entry:\n  b\n  c\n  d\n", out)

    def test_missing_inputs_fail(self) -> None:
        rc, _, err = self.run_main(None)
        self.assertEqual(rc, 1)
        self.assertIn("no kernel ELF", err)
        rc, _, err = self.run_main(elf([(".text", css.SHF_ALLOC, 0, b"")]))
        self.assertEqual(rc, 1)
        self.assertIn("no .stack_sizes section", err)
        alloc = elf([(".stack_sizes", css.SHF_ALLOC, 0x1000, self.sizes(entry=8))])
        rc, _, err = self.run_main(alloc)
        self.assertEqual(rc, 1)
        self.assertIn("is allocated", err)
        blob = elf([(".stack_sizes", 0, 0, self.sizes(entry=8))])
        rc, _, err = self.run_main(blob, LISTING.replace("vibeos_syscall_entry", "other"))
        self.assertEqual(rc, 1)
        self.assertIn("no root vibeos_syscall_entry", err)


class TestBootDoc(unittest.TestCase):
    def test_boot_md_states_the_bound(self) -> None:
        """BOOT.md §3.5 records `BOUND_BYTES` (ROADMAP §10.2)."""
        text = (ROOT / "docs/BOOT.md").read_text(encoding="utf-8")
        m = re.search(r"^## 3\.5 .*?(?=^## )", text, re.S | re.M)
        self.assertIsNotNone(m, "BOOT.md has no §3.5")
        assert m is not None
        self.assertIn(f"`BOUND_BYTES` = {css.BOUND_BYTES} bytes", m.group(0))


if __name__ == "__main__":
    unittest.main()
