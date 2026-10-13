"""Host tests for scripts/check_arch.py (the seam and the x86 audit, ROADMAP §10.3)."""

from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import check_arch
from scripts.check_arch import audit, check, fenced_lines, strip_code

# The fence attribute, built in pieces so the fixtures read plainly.
FENCE = '#[cfg(target_arch = "x86_64")]'

PORTABILITY = """# 11. Portability

## 11.1 The seam

| Concern | Seam | x86_64 | aarch64 | ROADMAP |
|---|---|---|---|---|
| Boot handover: the handshake | trait (`BootHandover`) | Limine | Limine | §10.3 |
| Early console | port module | 16550 | PL011 | §11.1 |
| Syscall instruction ([§5.10](X.md#a)), numbers | trait (`SyscallAbi`) | x | y | §10.3 |

## 11.2 Next
"""

CORE_ARCH = """pub mod x86_64;
pub use crate::trap::SyscallAbi;
pub trait BootHandover {}
pub trait Port: BootHandover + SyscallAbi {}
pub const fn assert_port<A: Port>() {}
"""

ARCH_MD = """# Arch

## Seam rows

| Concern | Seam | Portable side | x86_64 hardware half | x86_64 pure half | Stub |
|---|---|---|---|---|---|
| Boot handover | trait | `crates/core/src/arch/mod.rs` | `src/arch/x86_64/mod.rs` | none | \
`crates/core/src/arch/stub.rs` |
| Early console | port module | none | `src/arch/x86_64/mod.rs` | none | none |
| Syscall instruction (§5.10), numbers | trait | none | `src/arch/current.rs` | none | none |

## Fenced sites

| File | Items | §11.1 row | Why fenced, not moved |
|---|---|---|---|
{fenced}

## Audit

The grep.
"""

PURE = ("desc", "vectors", "pic", "apic", "paging")


class Tree:
    """A minimal repository that passes, which each test then breaks."""

    def __init__(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        self.fenced_rows: list[str] = []
        self.write("docs/PORTABILITY.md", PORTABILITY)
        self.write("crates/core/src/arch/mod.rs", CORE_ARCH)
        self.write("crates/core/src/arch/stub.rs", "const _: () = assert_port::<Arch>();\n")
        self.write("src/arch/current.rs", "const _: () = assert_port::<Arch>();\n")
        self.write("src/arch/x86_64/mod.rs", 'fn f() { unsafe { core::arch::asm!("hlt") } }\n')
        for n in PURE:
            self.write(f"crates/core/src/arch/x86_64/{n}.rs", "\n")
        self.write("crates/core/src/lib.rs", "pub mod arch;\n")
        self.write("src/main.rs", "mod arch;\n")
        self.write_arch_md()

    def close(self) -> None:
        self._dir.cleanup()

    def write(self, rel: str, text: str) -> None:
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")

    def write_arch_md(self, text: str | None = None) -> None:
        if text is None:
            text = ARCH_MD.replace("{fenced}", "\n".join(self.fenced_rows)) + "\n"
            text += "Pure: " + ", ".join(f"`crates/core/src/arch/x86_64/{n}.rs`" for n in PURE)
            text += "\n"
        self.write("docs/ARCH.md", text)

    def list_fenced(self, rel: str) -> None:
        self.fenced_rows.append(f"| `{rel}` | f | Idle | x86 only |")
        self.write_arch_md()

    def errors(self) -> list[str]:
        return check(self.root)


class ArchTest(unittest.TestCase):
    def setUp(self) -> None:
        self.t = Tree()
        self.addCleanup(self.t.close)

    def assertRule(self, rule: str) -> list[str]:
        errs = self.t.errors()
        hits = [e for e in errs if f": {rule}: " in e]
        self.assertTrue(hits, f"no {rule} failure in {errs}")
        return hits


class TestStrip(ArchTest):
    def test_comment_and_string_hits_ignored(self) -> None:
        text = (
            "// x86 asm!(\"cli\") in a comment\n"
            "/* cr3 /* nested */ EFER */\n"
            'const S: &str = "x86_64 wrmsr";\n'
            'const R: &str = r#"cr4 "GS_BASE" "#;\n'
            "const C: char = 'x';\n"
            "fn f<'a>(x: &'a u8) -> &'a u8 { x }\n"
        )
        code = strip_code(text)
        self.assertNotRegex(code, r"x86|cr3|EFER|wrmsr|cr4|GS_BASE|asm!")
        self.assertIn("fn f<'a>(x: &'a u8)", code)
        self.assertEqual(code.count("\n"), text.count("\n"))
        self.t.write("src/log/a.rs", text)
        self.assertEqual(audit(self.t.root), [])
        self.assertEqual(self.t.errors(), [])


class TestFences(ArchTest):
    def test_item_fence(self) -> None:
        text = (
            f"{FENCE}\n#[inline]\nfn f<'a>(s: &'a str) {{\n"
            "    let _ = (\"}{\", '}', r#\"}\"#);\n    x86::halt();\n}\n"
            "fn g() { x86::halt(); }\n"
        )
        lines = fenced_lines(text)
        self.assertEqual(lines, {1, 2, 3, 4, 5, 6})
        self.t.write("src/log/a.rs", text)
        self.t.list_fenced("src/log/a.rs")
        errs = self.assertRule("audit")
        self.assertEqual(len(errs), 1)
        self.assertIn("src/log/a.rs:7:", errs[0])

    def test_statement_fence(self) -> None:
        text = (
            "fn f() {\n"
            f"    {FENCE}\n"
            "    let fs = x86::rdmsr(1);\n"
            f"    {FENCE}\n"
            "    unsafe {\n"
            "        x86::wrmsr(1, 2);\n"
            "    }\n"
            "    x86::halt();\n"
            "}\n"
        )
        self.assertEqual(fenced_lines(text), {2, 3, 4, 5, 6, 7})
        self.t.write("src/log/a.rs", text)
        self.t.list_fenced("src/log/a.rs")
        errs = self.assertRule("audit")
        self.assertEqual([e.split(": ")[0] for e in errs], ["src/log/a.rs:8"])

    def test_array_element_fence(self) -> None:
        text = (
            "const TESTS: &[Test] = &[\n"
            '    t("a", a),\n'
            f"    {FENCE}\n"
            '    t("gdt", x86::gdt_ok),\n'
            '    t("b", b),\n'
            "];\n"
        )
        self.assertEqual(fenced_lines(text), {3, 4})
        self.t.write("src/log/a.rs", text)
        self.t.list_fenced("src/log/a.rs")
        self.assertEqual(self.t.errors(), [])

    def test_use_and_mod_line_fence(self) -> None:
        self.t.write(
            "src/proc/mod.rs",
            f"{FENCE}\nuse crate::x86;\n{FENCE}\npub mod syscall_init;\nmod other;\n",
        )
        self.t.write("src/proc/syscall_init.rs", "fn f() { x86::wrmsr(STAR, 0); }\n")
        self.t.write("src/proc/other.rs", "fn g() { x86::halt(); }\n")
        self.t.list_fenced("src/proc/mod.rs")
        self.t.list_fenced("src/proc/syscall_init.rs")
        self.t.list_fenced("src/proc/other.rs")
        errs = self.assertRule("audit")
        self.assertEqual([e.split(": ")[0] for e in errs], ["src/proc/other.rs:1"])

    def test_inner_attribute_fence(self) -> None:
        self.t.write("src/dev/pio.rs", '#![cfg(target_arch = "x86_64")]\nfn f() { x86::outb(); }\n')
        self.t.list_fenced("src/dev/pio.rs")
        self.assertEqual(self.t.errors(), [])
        self.assertTrue(all(h.fenced for h in audit(self.t.root)))

    def test_nested_module_fence(self) -> None:
        self.t.write("src/console/mod.rs", f"{FENCE}\nmod kbd;\n")
        self.t.write("src/console/kbd.rs", "mod ps2;\nfn f() { x86::inb(0x60); }\n")
        self.t.write("src/console/kbd/ps2.rs", "fn g() { x86::outb(0x64, 0); }\n")
        self.t.list_fenced("src/console/kbd.rs")
        self.t.list_fenced("src/console/kbd/ps2.rs")
        self.assertEqual(self.t.errors(), [])

    def test_cfg_not_is_not_a_fence(self) -> None:
        text = (
            '#[cfg(not(target_arch = "x86_64"))]\nfn f() { x86::halt(); }\n'
            '#[cfg(any(target_arch = "x86_64"))]\nfn g() { x86::halt(); }\n'
        )
        self.assertEqual(fenced_lines(text), set())
        self.t.write("src/log/a.rs", text)
        self.t.list_fenced("src/log/a.rs")
        self.assertEqual(len(self.assertRule("audit")), 2)


class TestAudit(ArchTest):
    def test_unfenced_kernel_hit_fails(self) -> None:
        for token in ("asm!(\"hlt\")", "x86::halt()", "read_cr3()", "IA32_EFER", "cr2",
                      "CR4", "LSTAR", "MSR_FOO", "FS_BASE"):
            with self.subTest(token=token):
                self.t.write("src/log/a.rs", f"fn f() {{ let _ = {token}; }}\n")
                errs = self.t.errors()
                if token == "read_cr3()":
                    # `_` is a word character: `read_cr3` is no hit.
                    self.assertEqual(errs, [])
                else:
                    self.assertEqual(len(errs), 2, errs)  # the hit and the unlisted file
                    self.assertTrue(any(": audit: " in e and "src/log/a.rs:1:" in e
                                        for e in errs), errs)

    def test_core_hit_fails_even_fenced(self) -> None:
        self.t.write("crates/core/src/log/a.rs", f"{FENCE}\nfn f() {{ x86::halt(); }}\n")
        errs = self.assertRule("audit")
        self.assertIn("vibeos-core", errs[0])

    def test_unlisted_fenced_file_fails(self) -> None:
        self.t.write("src/log/a.rs", f"{FENCE}\nfn f() {{ x86::halt(); }}\n")
        errs = self.assertRule("audit")
        self.assertIn("not listed", errs[0])

    def test_stale_fenced_row_fails(self) -> None:
        self.t.write("src/log/a.rs", "fn f() {}\n")
        self.t.list_fenced("src/log/a.rs")
        errs = self.assertRule("audit")
        self.assertIn("no hit", errs[0])

    def test_arch_dirs_exempt(self) -> None:
        self.t.write("src/arch/x86_64/cpu.rs", "fn f() { x86::wrmsr(IA32_EFER, 0); }\n")
        self.t.write("crates/core/src/arch/x86_64/desc.rs", "const STAR: u64 = 0;\n")
        self.t.write_arch_md(
            (self.t.root / "docs/ARCH.md").read_text() + "`src/arch/x86_64/cpu.rs`\n"
        )
        self.assertEqual(audit(self.t.root), [])
        self.assertEqual(self.t.errors(), [])


class TestSeam(ArchTest):
    def test_rows_match_seam_table(self) -> None:
        self.assertEqual(self.t.errors(), [])
        text = (self.t.root / "docs/ARCH.md").read_text()
        self.t.write_arch_md(text.replace("| Early console |", "| Early consoles |"))
        errs = self.assertRule("rows")
        self.assertIn("seam row 2", errs[0])

    def test_missing_row_fails(self) -> None:
        text = (self.t.root / "docs/ARCH.md").read_text()
        lines = [ln for ln in text.splitlines() if not ln.startswith("| Early console |")]
        self.t.write_arch_md("\n".join(lines) + "\n")
        self.assertRule("rows")

    def test_trait_missing_from_port_fails(self) -> None:
        self.t.write(
            "crates/core/src/arch/mod.rs",
            CORE_ARCH.replace("BootHandover + SyscallAbi", "SyscallAbi"),
        )
        errs = self.assertRule("seam")
        self.assertIn("BootHandover", errs[0])
        self.t.write("src/arch/current.rs", "\n")
        self.assertTrue(any("src/arch/current.rs" in e for e in self.assertRule("seam")))

    def test_dyn_seam_trait_fails(self) -> None:
        self.t.write("crates/core/src/log/a.rs", "fn f(_: &dyn BootHandover) {}\n")
        self.t.write("src/log/b.rs", "fn g(_: &dyn vibeos::arch::Port) {}\n// dyn Port\n")
        errs = self.assertRule("dyn")
        self.assertEqual(len(errs), 2, errs)

    def test_missing_path_fails(self) -> None:
        text = (self.t.root / "docs/ARCH.md").read_text()
        self.t.write_arch_md(text + "`src/arch/x86_64/gone.rs`\n")
        errs = self.assertRule("paths")
        self.assertIn("gone.rs", errs[0])
        (self.t.root / "crates/core/src/arch/x86_64/pic.rs").unlink()
        self.assertRule("pure")

    def test_unlisted_port_file_fails(self) -> None:
        self.t.write("src/arch/x86_64/new.rs", "\n")
        errs = self.assertRule("paths")
        self.assertIn("src/arch/x86_64/new.rs", errs[0])

    def test_unlisted_aarch64_port_file_fails(self) -> None:
        self.t.write("src/arch/aarch64/new.rs", "\n")
        errs = self.assertRule("paths")
        self.assertIn("src/arch/aarch64/new.rs", errs[0])


class TestSkips(ArchTest):
    def test_aarch64_row_needs_counterpart_or_reason(self) -> None:
        self.t.write(
            "tests/harness/skips.toml",
            '[[skip]]\nname = "ac_clear_user_popf"\nreason = "x"\narch = "aarch64"\n',
        )
        errs = [e for e in self.t.errors() if ": skips:" in e]
        self.assertTrue(errs, self.t.errors())
        self.assertIn("neither counterpart nor no_counterpart", errs[0])

    def test_aarch64_row_with_counterpart_passes(self) -> None:
        self.t.write(
            "tests/harness/skips.toml",
            '[[skip]]\nname = "ist_gs_sign"\nreason = "no swapgs"\n'
            'arch = "aarch64"\ncounterpart = "el0_svc_eret"\n',
        )
        self.assertEqual([e for e in self.t.errors() if ": skips:" in e], [])

    def test_aarch64_row_with_no_counterpart_passes(self) -> None:
        self.t.write(
            "tests/harness/skips.toml",
            '[[skip]]\nname = "ac_clear_user_popf"\nreason = "EL0 cannot write PAN"\n'
            'arch = "aarch64"\nno_counterpart = "EL0 cannot write PAN"\n',
        )
        self.assertEqual([e for e in self.t.errors() if ": skips:" in e], [])


class TestRepo(unittest.TestCase):
    def test_repo_passes(self) -> None:
        self.assertEqual(check(), [])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_arch.main(["--list"]), 0)
        self.assertNotIn("UNFENCED", out.getvalue())


if __name__ == "__main__":
    unittest.main()
