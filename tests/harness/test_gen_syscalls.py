"""Host tests for scripts/gen_syscalls.py (ROADMAP §10.5, C-SYSTABLE)."""

from __future__ import annotations

import contextlib
import io
import re
import shutil
import tempfile
import unittest
from pathlib import Path

from scripts import gen_syscalls
from scripts.gen_syscalls import (
    EMITTERS,
    KERNEL_OUT,
    KERROR,
    ROOT,
    SYSCALL_MD,
    TABLE,
    USER_ERRNO_OUT,
    USER_OUT,
    X86_OUT,
    ErrnoRow,
    TableError,
    emit_errno_table,
    generate,
    load_errno_table,
    main,
    parse,
    parse_errno_table,
)

FIXTURE = """\
[[syscall]]
name = "read"
x86_64 = 0
aarch64 = 63
args = [
  { name = "fd", type = "unsigned int" },
  { name = "buf", type = "char *", ptr = "buf", len = "count", dir = "out", when = "late" },
  { name = "count", type = "size_t" },
]
note = "a note"

[[syscall]]
name = "open"
x86_64 = 2
args = [
  { name = "pathname", type = "const char *", ptr = "cstr", when = "first" },
  { name = "flags", type = "int" },
  { name = "mode", type = "umode_t" },
]

[[syscall]]
name = "clone"
x86_64 = 56
aarch64 = 220
args = [
  { name = "flags", type = "unsigned long" },
  { name = "stack", type = "unsigned long" },
  { name = "parent_tid", type = "int *", ptr = "fixed", size = 4, dir = "out", when = "w" },
  { name = "child_tid", type = "int *", ptr = "fixed", size = 4, dir = "out", when = "w" },
  { name = "tls", type = "unsigned long" },
]
aarch64_order = ["flags", "stack", "parent_tid", "tls", "child_tid"]

[[syscall]]
name = "wait4"
x86_64 = 61
aarch64 = 260
args = [
  { name = "pid", type = "pid_t" },
  { name = "wstatus", type = "int *", ptr = "fixed", size = 4, dir = "out", when = "w" },
  { name = "options", type = "int" },
  { name = "rusage", type = "struct rusage *", unread = "ROADMAP §13.7" },
]
"""

DOC = """\
# Doc

before

<!-- gen_syscalls: begin syscall-table -->
old table
<!-- gen_syscalls: end syscall-table -->

after

<!-- gen_syscalls: begin errno-table -->
old errno table
<!-- gen_syscalls: end errno-table -->

end
"""

KERROR_FIXTURE = """\
//! A doc line.

errno_table! {
    // A comment line.
    Perm = 1, "EPERM", "`mmap` below page 0";

    Inval = 22, "EINVAL", "a bad \\"argument\\"";
}
"""


def fixture_root(doc: str = DOC, kerror: str = KERROR_FIXTURE) -> Path:
    root = Path(tempfile.mkdtemp())
    (root / SYSCALL_MD).parent.mkdir(parents=True)
    (root / SYSCALL_MD).write_text(doc, encoding="utf-8")
    (root / KERROR).parent.mkdir(parents=True)
    (root / KERROR).write_text(kerror, encoding="utf-8")
    return root


def row(body: str, name: str = "x", nr: int = 1) -> str:
    return f'[[syscall]]\nname = "{name}"\nx86_64 = {nr}\n{body}\n'


class GenerateTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = fixture_root()
        self.addCleanup(shutil.rmtree, self.root)
        self.out = generate(FIXTURE, self.root)

    def test_emits_every_output(self) -> None:
        self.assertEqual(set(self.out), {e.path for e in EMITTERS})
        self.assertEqual(set(self.out), {KERNEL_OUT, X86_OUT, USER_OUT, SYSCALL_MD, USER_ERRNO_OUT})
        kernel = self.out[KERNEL_OUT]
        x86 = self.out[X86_OUT]
        self.assertTrue(kernel.startswith("// @generated"))
        self.assertTrue(x86.startswith("// @generated"))
        self.assertIn("pub const SYS_READ: u64 = 0;", x86)
        self.assertIn("pub const SYS_READ: u64 = 63;", kernel)
        self.assertNotIn("pub mod x86_64", kernel)
        self.assertIn("fn read(&mut self, fd: u32, buf: u64, count: usize) -> SysResult;", kernel)
        self.assertIn("kind: PtrKind::Buf { len_from: 2 },", kernel)
        self.assertIn("kind: PtrKind::Unread,", kernel)
        self.assertIn("NrRule::SignExtendEax", x86)
        self.assertIn("NrRule::Low32", kernel)
        # `open` has no aarch64 number: aarch64's `call` returns ENOSYS for it.
        self.assertIn("Sys::Open => Err(KError::NoSys),", kernel)
        user = self.out[USER_OUT]
        self.assertIn("pub const SYS_OPEN: usize = 2;", user)
        self.assertIn(
            "pub unsafe fn read(fd: u32, buf: *mut u8, count: usize) -> Result<usize, Errno>", user
        )
        self.assertIn("pub fn open(pathname: *const u8, flags: i32, mode: u16)", user)
        self.assertIn("rusage: *mut c_void", user)
        self.assertIn('b"wait4" => Some(Sys::Wait4),', user)
        md = self.out[SYSCALL_MD]
        self.assertIn(
            "| 0 | 63 | `read` | 3 | `unsigned int fd`, `char *buf`, `size_t count` |", md
        )
        self.assertIn("| 2 | — | `open` |", md)
        self.assertIn("`rusage`: not read (ROADMAP §13.7)", md)

    def test_block_changes_only_between_markers(self) -> None:
        md = self.out[SYSCALL_MD]
        begin = "<!-- gen_syscalls: begin syscall-table -->"
        end = "<!-- gen_syscalls: end syscall-table -->"
        self.assertTrue(md.startswith("# Doc\n\nbefore\n\n" + begin + "\n"))
        self.assertIn(end + "\n\nafter\n\n<!-- gen_syscalls: begin errno-table -->\n", md)
        self.assertTrue(md.endswith("<!-- gen_syscalls: end errno-table -->\n\nend\n"))
        self.assertNotIn("old table", md)
        self.assertNotIn("old errno table", md)
        # Generating over its own output changes nothing.
        (self.root / SYSCALL_MD).write_text(md, encoding="utf-8")
        self.assertEqual(generate(FIXTURE, self.root)[SYSCALL_MD], md)

    def test_missing_markers_is_an_error(self) -> None:
        for doc in (
            "no markers\n",
            "<!-- gen_syscalls: begin syscall-table -->\n",
            "<!-- gen_syscalls: end syscall-table -->\n"
            "<!-- gen_syscalls: begin syscall-table -->\n",
            DOC + DOC,
        ):
            root = fixture_root(doc)
            self.addCleanup(shutil.rmtree, root)
            with self.assertRaisesRegex(TableError, "markers"):
                generate(FIXTURE, root)

    def test_clone_aarch64_order_swaps_registers(self) -> None:
        kernel = self.out[KERNEL_OUT]
        x86 = self.out[X86_OUT]
        arm = kernel[kernel.index("pub mod aarch64") :]
        want_x86 = "h.clone(regs[0], regs[1], regs[2], regs[3], regs[4])"
        want_arm = "h.clone(regs[0], regs[1], regs[2], regs[4], regs[3])"
        self.assertIn(want_x86, x86)
        self.assertIn(want_arm, arm)
        # child_tid is argument 3: register 3 on x86_64, register 4 on aarch64;
        # tls, argument 4, the other way round.
        rows = parse(FIXTURE).rows
        clone = next(r for r in rows if r.name == "clone")
        self.assertEqual(clone.aarch64_order, (0, 1, 2, 4, 3))

    def test_c_type_casts(self) -> None:
        x86 = self.out[X86_OUT]
        self.assertIn("h.read(regs[0] as u32, regs[1], regs[2] as usize)", x86)
        self.assertIn("h.open(regs[0], regs[1] as i32, regs[2] as u16)", x86)
        self.assertIn("h.wait4(regs[0] as i32, regs[1], regs[2] as i32, regs[3])", x86)


class ErrnoTableTest(unittest.TestCase):
    def test_two_rows_exact_output(self) -> None:
        rows = [
            ErrnoRow("Inval", 22, "EINVAL", "bad argument"),
            ErrnoRow("Perm", 1, "EPERM", ""),
        ]
        self.assertEqual(
            emit_errno_table(rows),
            "\n| Name | Value | Used |\n|------|------:|------|\n"
            "| `EPERM` | 1 | |\n| `EINVAL` | 22 | bad argument |\n\n",
        )

    def test_block_between_markers_and_user_constants(self) -> None:
        root = fixture_root()
        self.addCleanup(shutil.rmtree, root)
        out = generate(FIXTURE, root)
        md = out[SYSCALL_MD]
        self.assertIn(
            "<!-- gen_syscalls: begin errno-table -->\n\n| Name | Value | Used |\n"
            "|------|------:|------|\n| `EPERM` | 1 | `mmap` below page 0 |\n"
            '| `EINVAL` | 22 | a bad "argument" |\n\n<!-- gen_syscalls: end errno-table -->',
            md,
        )
        user = out[USER_ERRNO_OUT]
        self.assertTrue(user.startswith("// @generated by scripts/gen_syscalls.py"))
        self.assertIn("pub const EINVAL: Errno = Errno(22);", user)

    def test_parse_skips_blanks_and_comments(self) -> None:
        rows = parse_errno_table(KERROR_FIXTURE, "k")
        self.assertEqual(
            rows,
            [
                ErrnoRow("Perm", 1, "EPERM", "`mmap` below page 0"),
                ErrnoRow("Inval", 22, "EINVAL", 'a bad "argument"'),
            ],
        )

    def test_each_parse_error(self) -> None:
        cases = {
            "not an errno row": 'errno_table! {\n    Perm = 1, "EPERM";\n}\n',
            "want one line": "no table\n",
            "no closing": 'errno_table! {\n    Perm = 1, "EPERM", "";\n',
            "repeated": 'errno_table! {\n    Perm = 1, "EPERM", "";\n    Other = 1, "EX", "";\n}\n',
            "outside 1 to 4095": 'errno_table! {\n    Big = 4096, "EBIG", "";\n}\n',
            "no rows": "errno_table! {\n}\n",
        }
        for want, text in cases.items():
            with self.subTest(want=want), self.assertRaisesRegex(TableError, want):
                parse_errno_table(text, "k")

    def test_the_tree_table_loads(self) -> None:
        rows = load_errno_table(ROOT / KERROR)
        by_name = {r.name: r.value for r in rows}
        self.assertEqual(by_name["EINVAL"], 22)
        self.assertEqual(by_name["ENOSYS"], 38)

    def test_check_fails_after_a_hand_edit_of_a_section_2_row(self) -> None:
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root)
        for p in [TABLE, KERROR] + [e.path for e in EMITTERS]:
            (root / p).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(ROOT / p, root / p)
        md = root / SYSCALL_MD
        text = md.read_text(encoding="utf-8")
        row = "| `ENOSYS` | 38 | unknown number |"
        self.assertIn(row, text)
        md.write_text(text.replace(row, "| `ENOSYS` | 38 | a hand edit |"), encoding="utf-8")
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(main(["--check"], root), 1)
        self.assertIn(str(SYSCALL_MD), err.getvalue())


class CheckTest(unittest.TestCase):
    def test_check_passes_on_the_tree(self) -> None:
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(main(["--check"]), 0)

    def test_check_fails_on_a_changed_output(self) -> None:
        for em in EMITTERS:
            with self.subTest(path=str(em.path)):
                root = Path(tempfile.mkdtemp())
                self.addCleanup(shutil.rmtree, root)
                for p in [TABLE, KERROR] + [e.path for e in EMITTERS]:
                    (root / p).parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy(ROOT / p, root / p)
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(main(["--check"], root), 0)
                target = root / em.path
                data = bytearray(target.read_bytes())
                marker = f"gen_syscalls: begin {em.block}".encode()
                i = data.index(b"|", data.index(marker)) if em.block else len(data) // 2
                data[i] = ord("#") if data[i] != ord("#") else ord("|")
                target.write_bytes(bytes(data))
                err = io.StringIO()
                with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(main(["--check"], root), 1)
                self.assertIn(str(em.path), err.getvalue())
                # Without --check it rewrites the output, and --check passes again.
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(main([], root), 0)
                    self.assertEqual(main(["--check"], root), 0)

    def test_table_error_exits_1(self) -> None:
        root = fixture_root()
        self.addCleanup(shutil.rmtree, root)
        (root / TABLE).parent.mkdir(parents=True)
        (root / TABLE).write_text(row('args = [{ name = "a", type = "float" }]'))
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(main(["--check"], root), 1)
        self.assertIn("unknown C type", err.getvalue())


class ValidationTest(unittest.TestCase):
    CASES: list[tuple[str, str, str]] = [
        (
            "duplicate name",
            row("args = []", "a", 1) + row("args = []", "a", 2),
            "duplicate name",
        ),
        (
            "duplicate x86_64 number",
            row("args = []", "a", 1) + row("args = []", "b", 1),
            "duplicate x86_64 number 1",
        ),
        (
            "duplicate aarch64 number",
            row("aarch64 = 5\nargs = []", "a", 1) + row("aarch64 = 5\nargs = []", "b", 2),
            "duplicate aarch64 number 5",
        ),
        ("unknown C type", row('args = [{ name = "a", type = "float" }]'), "unknown C type"),
        (
            "unknown pointee",
            row('args = [{ name = "a", type = "float *", ptr = "cstr", when = "w" }]'),
            "unknown C type",
        ),
        (
            "arity over 6",
            row("args = [" + ", ".join(f'{{ name = "a{i}", type = "int" }}' for i in range(7))
                + "]"),
            "arity 7 is over 6",
        ),
        (
            "len names no argument",
            row(
                'args = [{ name = "b", type = "char *", ptr = "buf", len = "n", dir = "in",'
                ' when = "w" }]'
            ),
            "len 'n' names no integer argument",
        ),
        (
            "len names a pointer",
            row(
                'args = [{ name = "b", type = "char *", ptr = "buf", len = "c", dir = "in",'
                ' when = "w" }, { name = "c", type = "char *", ptr = "cstr", when = "w" }]'
            ),
            "len 'c' names no integer argument",
        ),
        (
            "null on a non-pointer",
            row('args = [{ name = "a", type = "int", null = true }]'),
            "null on a non-pointer",
        ),
        (
            "ptr on a non-pointer",
            row('args = [{ name = "a", type = "int", ptr = "cstr" }]'),
            "ptr on a non-pointer",
        ),
        (
            "undeclared pointer",
            row('args = [{ name = "a", type = "char *" }]'),
            "neither ptr nor unread",
        ),
        (
            "unknown ptr kind",
            row('args = [{ name = "a", type = "char *", ptr = "blob", when = "w" }]'),
            "unknown ptr",
        ),
        (
            "pointer without when",
            row('args = [{ name = "a", type = "char *", ptr = "cstr" }]'),
            "when its handler copies",
        ),
        (
            "fixed without size",
            row('args = [{ name = "a", type = "int *", ptr = "fixed", dir = "out", when = "w" }]'),
            "declares its size",
        ),
        (
            "buf without dir",
            row(
                'args = [{ name = "b", type = "char *", ptr = "buf", len = "n", when = "w" },'
                ' { name = "n", type = "size_t" }]'
            ),
            "dir must be one of",
        ),
        (
            "aarch64_order not a permutation",
            row(
                'args = [{ name = "a", type = "int" }, { name = "b", type = "int" }]\n'
                'aarch64_order = ["a", "a"]'
            ),
            "not a permutation",
        ),
        ("Rust keyword row", row("args = []", "type"), "Rust keyword"),
        (
            "Rust keyword argument",
            row('args = [{ name = "fn", type = "int" }]'),
            "Rust keyword",
        ),
        ("bad name", row("args = []", "Read"), "not a lowercase identifier"),
        ("unknown row key", row("args = []\nflags = 1"), "unknown keys"),
        (
            "unknown argument key",
            row('args = [{ name = "a", type = "int", width = 4 }]'),
            "unknown keys",
        ),
        ("no rows", "", "no \\[\\[syscall\\]\\] rows"),
    ]

    def test_each_validation_error(self) -> None:
        for what, text, message in self.CASES:
            with self.subTest(what=what), self.assertRaisesRegex(TableError, message):
                parse(text)

    def test_the_tree_table_parses(self) -> None:
        rows = parse((ROOT / TABLE).read_text(encoding="utf-8")).rows
        names = {r.name for r in rows}
        self.assertLessEqual({"read", "write", "open", "execve", "wait4", "psinfo"}, names)
        for name, arg in (
            ("open", "pathname"),
            ("execve", "pathname"),
            ("execve", "argv"),
            ("execve", "envp"),
            ("wait4", "wstatus"),
        ):
            r = next(r for r in rows if r.name == name)
            a = next(a for a in r.args if a.name == arg)
            self.assertIn(a.kind, ("buf", "fixed", "cstr", "strvec"), f"{name}.{arg}")

    def test_script_docstring_names_its_outputs(self) -> None:
        doc = gen_syscalls.__doc__ or ""
        for p in (KERNEL_OUT, X86_OUT, USER_OUT, USER_ERRNO_OUT, KERROR):
            self.assertIn(str(p), doc)
        self.assertTrue(re.search(r"--check", doc))


if __name__ == "__main__":
    unittest.main()
