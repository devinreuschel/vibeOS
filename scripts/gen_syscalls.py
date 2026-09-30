#!/usr/bin/env python3
"""Generate the syscall table's outputs from one table (ROADMAP §10.5, C-SYSTABLE).

`crates/core/src/proc/syscalls.toml` holds each syscall's number per
architecture, its arguments' C types in order, and its pointer declarations
(the file's header gives the schema). This script writes, from it:

- `crates/core/src/proc/syscall_table.rs`: `Sys`, `ROWS`, `trait Handlers`,
  aarch64's numbers, the `TABLE` dispatch indexes and the typed
  `call`/`dispatch`, plus the host tests' `Recorder`;
- `crates/core/src/arch/x86_64/syscall.rs`: the same for x86_64, in its
  port's pure half, which `SyscallAbi::dispatch` reaches (PORTABILITY
  §11.1);
- `user/src/arch/x86_64/sys.rs`: the numbers, `Sys::from_name`, and one
  stub per row with an x86_64 number;
- the block between `<!-- gen_syscalls: begin syscall-table -->` and
  `<!-- gen_syscalls: end syscall-table -->` in docs/SYSCALL.md §3.

From the `errno_table! { … }` block of `crates/core/src/kerror.rs` (ROADMAP
§10.4, C-KERROR), one row per line, `Variant = N, "ENAME", "Used text";`:

- the block between `<!-- gen_syscalls: begin errno-table -->` and
  `<!-- gen_syscalls: end errno-table -->` in docs/SYSCALL.md §2;
- `user/src/errno.rs`: the user runtime's `Errno::ENAME` constants.

`EMITTERS` is the registry of outputs: a whole file, or a marked block in a
document.

    gen_syscalls.py [--check]

Without `--check` it writes every output that differs. With it, it writes
nothing and exits 1 naming each output that differs (`make check` runs it).
A table error exits 1 with `<table>: <row>: <error>` lines.
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
TABLE = Path("crates/core/src/proc/syscalls.toml")
KERNEL_OUT = Path("crates/core/src/proc/syscall_table.rs")
X86_OUT = Path("crates/core/src/arch/x86_64/syscall.rs")
USER_OUT = Path("user/src/arch/x86_64/sys.rs")
SYSCALL_MD = Path("docs/SYSCALL.md")
KERROR = Path("crates/core/src/kerror.rs")
USER_ERRNO_OUT = Path("user/src/errno.rs")

MAX_ARGS = 6

# C type -> (Rust type, `CType` variant, recorder `Val` variant, signed).
SCALARS: dict[str, tuple[str, str, bool]] = {
    "int": ("i32", "Int", True),
    "unsigned int": ("u32", "UInt", False),
    "long": ("i64", "Long", True),
    "unsigned long": ("u64", "ULong", False),
    "size_t": ("usize", "SizeT", False),
    "off_t": ("i64", "OffT", True),
    "pid_t": ("i32", "PidT", True),
    "umode_t": ("u16", "UmodeT", False),
}
# Pointee C base type -> Rust type, for the user stubs.
POINTEES: dict[str, str] = {"char": "u8", "void": "c_void"} | {
    k: v[0] for k, v in SCALARS.items()
}
PTR_KINDS = ("buf", "fixed", "cstr", "strvec")
DIRS = ("in", "out")

RUST_KEYWORDS = frozenset(
    "as async await break const continue crate dyn else enum extern false fn for gen if impl "
    "in let loop match mod move mut pub ref return self Self static struct super trait true "
    "try type unsafe use where while abstract become box do final macro override priv typeof "
    "unsized virtual yield".split()
)
NAME = re.compile(r"^[a-z_][a-z0-9_]*$")
ROW_KEYS = frozenset(("name", "x86_64", "aarch64", "args", "aarch64_order", "unsafe", "note"))
ARG_KEYS = frozenset(("name", "type", "ptr", "len", "size", "dir", "null", "when", "unread"))

# The table's types, emitted as they are into the kernel table, so the
# generated module depends on nothing but `KError` (check_cycles.py).
KERNEL_TYPES = """\
/// What a syscall handler returns: the result, or the error dispatch
/// returns as `-errno` (SYSCALL.md §2).
pub type SysResult = Result<usize, KError>;

/// One syscall's row of the generated table (`syscalls.toml`, ROADMAP §10.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub sys: Sys,
    pub name: &'static str,
    /// The arguments, in canonical (x86_64) register order; the arity is
    /// their count. Each architecture's number is in its [`NrTable`].
    pub args: &'static [Arg],
}

impl Row {
    /// The number of arguments.
    pub const fn arity(&self) -> usize {
        self.args.len()
    }
}

/// One argument of a [`Row`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arg {
    pub name: &'static str,
    pub ty: CType,
    /// How the handler copies through a pointer; `None` for an integer.
    pub ptr: Option<Ptr>,
}

/// An argument's C type in Linux's prototype. Dispatch cuts each register
/// to it: `int` and `pid_t` to `i32`, `unsigned int` to `u32`, `long` and
/// `off_t` to `i64`, `unsigned long` and a pointer to `u64`, `size_t` to
/// `usize`, `umode_t` to `u16` (SYSCALL.md §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CType {
    Int,
    UInt,
    Long,
    ULong,
    SizeT,
    OffT,
    PidT,
    UmodeT,
    Ptr,
}

impl CType {
    /// True for the integer types.
    pub const fn is_int(self) -> bool {
        !matches!(self, CType::Ptr)
    }
}

/// A pointer argument's declaration (SYSCALL.md §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ptr {
    pub kind: PtrKind,
    pub dir: Dir,
    /// NULL is valid and copies nothing.
    pub nullable: bool,
    /// Where the handler first copies through it, after which of Linux's
    /// checks.
    pub when: &'static str,
}

/// What a pointer argument points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtrKind {
    /// A buffer whose length is argument `len_from`.
    Buf { len_from: u8 },
    /// A value of `size` bytes.
    Fixed { size: u32 },
    /// A NUL-terminated string.
    CStr,
    /// A NULL-terminated array of C strings.
    StrVec,
    /// A pointer the kernel does not read yet; the row names the ROADMAP
    /// line that reads it.
    Unread,
}

/// Whether the kernel reads or writes through a pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
}

/// How an architecture's entry reads the syscall number (SYSCALL.md §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NrRule {
    /// x86_64: `eax` sign-extended, so the high half of `rax` is ignored
    /// and a negative number names no call.
    SignExtendEax,
    /// aarch64: the low 32 bits of `x8`, unsigned.
    Low32,
}

/// An architecture's number table: [`Sys`] by number.
#[derive(Debug)]
pub struct NrTable {
    rule: NrRule,
    slots: &'static [Option<Sys>],
}

impl NrTable {
    pub const fn new(rule: NrRule, slots: &'static [Option<Sys>]) -> NrTable {
        NrTable { rule, slots }
    }

    /// The call `raw`, the full number register, names; `None` for none
    /// (`ENOSYS`).
    pub fn lookup(&self, raw: u64) -> Option<Sys> {
        let n = match self.rule {
            NrRule::SignExtendEax => usize::try_from(raw as u32 as i32).ok()?,
            NrRule::Low32 => raw as u32 as usize,
        };
        self.slots.get(n).copied().flatten()
    }

    /// The table, indexed by number.
    pub const fn slots(&self) -> &'static [Option<Sys>] {
        self.slots
    }

    /// `sys`'s number in this table; `None` where Linux has none.
    pub fn number(&self, sys: Sys) -> Option<u64> {
        let n = self.slots.iter().position(|s| *s == Some(sys))?;
        u64::try_from(n).ok()
    }
}
"""

GENERATED = "@generated by scripts/gen_syscalls.py from crates/core/src/proc/syscalls.toml"


class TableError(Exception):
    """The table breaks a rule; the message names the row."""


@dataclass(frozen=True)
class CType:
    """One argument's C type."""

    text: str
    scalar: str | None  # the SCALARS key, or None for a pointer
    pointee: str = ""  # a pointer's Rust type in the user stubs

    @property
    def is_ptr(self) -> bool:
        return self.scalar is None

    @property
    def kernel_rust(self) -> str:
        return "u64" if self.scalar is None else SCALARS[self.scalar][0]

    @property
    def variant(self) -> str:
        return "Ptr" if self.scalar is None else SCALARS[self.scalar][1]

    @property
    def signed(self) -> bool:
        return self.scalar is not None and SCALARS[self.scalar][2]


@dataclass(frozen=True)
class Arg:
    name: str
    ty: CType
    kind: str | None = None  # PTR_KINDS, "unread", or None for a non-pointer
    dir: str = "in"
    nullable: bool = False
    len_from: int = -1
    size: int = 0
    when: str = ""


@dataclass(frozen=True)
class Row:
    name: str
    args: tuple[Arg, ...]
    x86_64: int | None
    aarch64: int | None
    aarch64_order: tuple[int, ...]  # register i holds args[aarch64_order[i]]
    unsafe: str
    note: str

    @property
    def variant(self) -> str:
        return "".join(p[:1].upper() + p[1:] for p in self.name.split("_"))

    def nr(self, arch: str) -> int | None:
        return self.x86_64 if arch == "x86_64" else self.aarch64


@dataclass(frozen=True)
class ErrnoRow:
    """One row of the `KError` table."""

    variant: str
    value: int
    name: str
    used: str


@dataclass
class Table:
    rows: list[Row] = field(default_factory=list)
    errno: list[ErrnoRow] = field(default_factory=list)


def parse_ctype(text: str, where: str) -> CType:
    """`text` as a C type: a scalar SCALARS names, or a pointer."""
    norm = " ".join(text.split())
    if not norm.endswith("*"):
        if norm not in SCALARS:
            raise TableError(f"{where}: unknown C type {text!r}")
        return CType(norm, norm)
    tokens = re.findall(r"\*|[A-Za-z_][A-Za-z0-9_]*", norm)
    if "".join(tokens) != norm.replace(" ", ""):
        raise TableError(f"{where}: unknown C type {text!r}")
    const = False
    if tokens and tokens[0] == "const":
        const = True
        tokens = tokens[1:]
    star = tokens.index("*") if "*" in tokens else len(tokens)
    base = " ".join(tokens[:star])
    rest = tokens[star:]
    if base.startswith("struct ") and NAME.match(base[7:]):
        rust = "c_void"
    elif base in POINTEES:
        rust = POINTEES[base]
    else:
        raise TableError(f"{where}: unknown C type {text!r}")
    i = 0
    while i < len(rest):
        if rest[i] != "*":
            raise TableError(f"{where}: unknown C type {text!r}")
        rust = ("*const " if const else "*mut ") + rust
        const = i + 1 < len(rest) and rest[i + 1] == "const"
        i += 2 if const else 1
    return CType(norm, None, rust)


def check_name(name: object, where: str) -> str:
    if not isinstance(name, str) or not NAME.match(name):
        raise TableError(f"{where}: name {name!r} is not a lowercase identifier")
    if name in RUST_KEYWORDS:
        raise TableError(f"{where}: name {name!r} is a Rust keyword")
    return name


def parse_arg(raw: Any, row: str, names: list[str], types: list[CType]) -> Arg:
    """One argument; `names`/`types` are the row's, for `len`."""
    if not isinstance(raw, dict):
        raise TableError(f"{row}: an argument is not a table")
    extra = set(raw) - ARG_KEYS
    name = check_name(raw.get("name"), f"{row}: argument")
    where = f"{row}.{name}"
    if extra:
        raise TableError(f"{where}: unknown keys {sorted(extra)}")
    ty = parse_ctype(str(raw.get("type", "")), where)
    null = raw.get("null", False)
    if not isinstance(null, bool):
        raise TableError(f"{where}: null must be true or false")
    if not ty.is_ptr:
        for key in ("ptr", "len", "size", "dir", "when", "unread"):
            if key in raw:
                raise TableError(f"{where}: {key} on a non-pointer")
        if "null" in raw:
            raise TableError(f"{where}: null on a non-pointer")
        return Arg(name, ty)
    if "unread" in raw:
        if set(raw) - {"name", "type", "unread", "null"}:
            raise TableError(f"{where}: an unread pointer declares nothing else but null")
        unread = raw["unread"]
        if not isinstance(unread, str) or not unread:
            raise TableError(f"{where}: unread names the ROADMAP line that reads it")
        return Arg(name, ty, "unread", nullable=null, when=f"not read ({unread})")
    kind = raw.get("ptr")
    if kind is None:
        raise TableError(f"{where}: a pointer declares neither ptr nor unread")
    if kind not in PTR_KINDS:
        raise TableError(f"{where}: unknown ptr {kind!r}")
    when = raw.get("when")
    if not isinstance(when, str) or not when:
        raise TableError(f"{where}: a pointer declares when its handler copies through it")
    direction = raw.get("dir", "in" if kind in ("cstr", "strvec") else None)
    if direction not in DIRS:
        raise TableError(f"{where}: dir must be one of {DIRS}")
    if kind in ("cstr", "strvec") and direction != "in":
        raise TableError(f"{where}: a {kind} is read, so its dir is in")
    len_from, size = -1, 0
    if kind == "buf":
        if "size" in raw:
            raise TableError(f"{where}: size on a buf")
        length = raw.get("len")
        if length not in names or types[names.index(length)].is_ptr:
            raise TableError(f"{where}: len {length!r} names no integer argument")
        len_from = names.index(length)
    elif "len" in raw:
        raise TableError(f"{where}: len on a {kind}")
    if kind == "fixed":
        size = raw.get("size", 0)
        if not isinstance(size, int) or isinstance(size, bool) or size <= 0:
            raise TableError(f"{where}: a fixed pointer declares its size in bytes")
    elif "size" in raw:
        raise TableError(f"{where}: size on a {kind}")
    return Arg(name, ty, kind, direction, null, len_from, size, when)


def parse(text: str) -> Table:
    """The table in `text`, or `TableError`."""
    try:
        doc = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise TableError(f"not TOML: {e}") from e
    if set(doc) - {"syscall"}:
        raise TableError(f"unknown top-level keys {sorted(set(doc) - {'syscall'})}")
    raws = doc.get("syscall", [])
    if not isinstance(raws, list) or not raws:
        raise TableError("no [[syscall]] rows")
    table = Table()
    seen: dict[str, set[int]] = {"x86_64": set(), "aarch64": set()}
    names: set[str] = set()
    for raw in raws:
        if not isinstance(raw, dict):
            raise TableError("a [[syscall]] row is not a table")
        name = check_name(raw.get("name"), "row")
        if set(raw) - ROW_KEYS:
            raise TableError(f"{name}: unknown keys {sorted(set(raw) - ROW_KEYS)}")
        if name in names:
            raise TableError(f"{name}: duplicate name")
        names.add(name)
        nrs: dict[str, int | None] = {}
        for arch in ("x86_64", "aarch64"):
            nr = raw.get(arch)
            if nr is not None and (not isinstance(nr, int) or isinstance(nr, bool) or nr < 0):
                raise TableError(f"{name}: {arch} number {nr!r} is not a non-negative integer")
            if nr is not None and nr in seen[arch]:
                raise TableError(f"{name}: duplicate {arch} number {nr}")
            if nr is not None:
                seen[arch].add(nr)
            nrs[arch] = nr
        raw_args = raw.get("args", [])
        if not isinstance(raw_args, list):
            raise TableError(f"{name}: args is not a list")
        if len(raw_args) > MAX_ARGS:
            raise TableError(f"{name}: arity {len(raw_args)} is over {MAX_ARGS}")
        arg_names: list[str] = []
        arg_types: list[CType] = []
        for a in raw_args:
            if isinstance(a, dict):
                n = check_name(a.get("name"), f"{name}: argument")
                if n in arg_names:
                    raise TableError(f"{name}.{n}: duplicate argument")
                arg_names.append(n)
                arg_types.append(parse_ctype(str(a.get("type", "")), f"{name}.{n}"))
        args = tuple(parse_arg(a, name, arg_names, arg_types) for a in raw_args)
        order_raw = raw.get("aarch64_order")
        if order_raw is None:
            order = tuple(range(len(args)))
        else:
            if not isinstance(order_raw, list) or sorted(map(str, order_raw)) != sorted(
                arg_names
            ):
                raise TableError(f"{name}: aarch64_order is not a permutation of its arguments")
            order = tuple(arg_names.index(str(n)) for n in order_raw)
        unsafe = raw.get("unsafe", "")
        note = raw.get("note", "")
        if not isinstance(unsafe, str) or not isinstance(note, str):
            raise TableError(f"{name}: unsafe and note are strings")
        table.rows.append(
            Row(name, args, nrs["x86_64"], nrs["aarch64"], order, unsafe, note.strip())
        )
    return table


# ----------------------------------------------------------- errno table

ERRNO_OPEN = "errno_table! {"
ERRNO_ROW = re.compile(
    r'^\s*([A-Z][A-Za-z0-9]*)\s*=\s*([0-9]+),\s*"(E[A-Z0-9]+)",\s*"((?:[^"\\]|\\.)*)";\s*$'
)


def parse_errno_table(text: str, where: str) -> list[ErrnoRow]:
    """The rows of the one `errno_table! { … }` invocation in `text`.

    Every line inside the block but a blank line or a `//` comment must be a
    row; each variant, value and name appears once, and a value is 1 to 4095.
    """
    lines = text.splitlines()
    opens = [i for i, ln in enumerate(lines) if ln.rstrip() == ERRNO_OPEN]
    if len(opens) != 1:
        raise TableError(f"{where}: want one line {ERRNO_OPEN!r}, found {len(opens)}")
    rows: list[ErrnoRow] = []
    for n in range(opens[0] + 1, len(lines)):
        raw = lines[n]
        if raw.rstrip() == "}":
            break
        body = raw.strip()
        if not body or body.startswith("//"):
            continue
        m = ERRNO_ROW.match(raw)
        if m is None:
            raise TableError(f"{where}:{n + 1}: not an errno row: {body!r}")
        used = re.sub(r"\\(.)", r"\1", m.group(4))
        rows.append(ErrnoRow(m.group(1), int(m.group(2)), m.group(3), used))
    else:
        raise TableError(f"{where}: {ERRNO_OPEN!r} block has no closing '}}' line")
    for attr in ("variant", "value", "name"):
        seen: set[object] = set()
        for r in rows:
            v = getattr(r, attr)
            if v in seen:
                raise TableError(f"{where}: {r.name}: {attr} {v!r} repeated")
            seen.add(v)
    for r in rows:
        if not 1 <= r.value <= 4095:
            raise TableError(f"{where}: {r.name}: value {r.value} outside 1 to 4095")
    if not rows:
        raise TableError(f"{where}: the errno table has no rows")
    return rows


def load_errno_table(path: Path) -> list[ErrnoRow]:
    """The `KError` table's rows from the Rust file at `path`."""
    return parse_errno_table(path.read_text(encoding="utf-8"), str(path))


def emit_errno_table(rows: Sequence[ErrnoRow]) -> str:
    """SYSCALL.md §2's errno table, in value order, for its marked block."""
    out = ["| Name | Value | Used |", "|------|------:|------|"]
    for r in sorted(rows, key=lambda r: r.value):
        out.append(f"| `{r.name}` | {r.value} |" + (f" {r.used} |" if r.used else " |"))
    return "\n" + "\n".join(out) + "\n\n"


def render_errno_md(table: Table) -> str:
    return emit_errno_table(table.errno)


def render_user_errno(table: Table) -> str:
    out = [
        f"// @generated by scripts/gen_syscalls.py from {KERROR.as_posix()}.",
        "// Do not edit: change the table and run the script (ROADMAP §10.4).",
        "",
        "//! Linux's errno values, as the kernel's `KError` table defines them",
        "//! (C-KERROR): `Errno::EBADF` is what a call that fails with `EBADF`",
        "//! returns.",
        "",
        "use crate::sys::Errno;",
        "",
        "impl Errno {",
    ]
    for r in sorted(table.errno, key=lambda r: r.value):
        out += [f"    /// Linux `{r.name}`.", f"    pub const {r.name}: Errno = Errno({r.value});"]
    out.append("}")
    return "\n".join(out) + "\n"


# ---------------------------------------------------------------- kernel


# rustfmt's defaults: `max_width` 100, and `fn_call_width` and `array_width`
# 60 (the arguments or elements alone), past which a list goes vertical.
MAX_WIDTH = 100
LIST_WIDTH = 60


def rust_list(indent: str, head: str, items: list[str], close: str, tail: str) -> list[str]:
    """`head` + items + `close` + `tail` as rustfmt lays it out: on one line
    when it fits, else one item per line."""
    joined = ", ".join(items)
    line = f"{indent}{head}{joined}{close}{tail}"
    if len(joined) <= LIST_WIDTH and len(line) <= MAX_WIDTH:
        return [line]
    return [f"{indent}{head}"] + [f"{indent}    {i}," for i in items] + [f"{indent}{close}{tail}"]


def rust_str(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def kernel_ptr(a: Arg) -> str:
    if a.kind is None:
        return "None"
    kind = {
        "buf": f"PtrKind::Buf {{ len_from: {a.len_from} }}",
        "fixed": f"PtrKind::Fixed {{ size: {a.size} }}",
        "cstr": "PtrKind::CStr",
        "strvec": "PtrKind::StrVec",
        "unread": "PtrKind::Unread",
    }[a.kind]
    d = "Dir::Out" if a.dir == "out" else "Dir::In"
    return (
        f"Some(Ptr {{\n                    kind: {kind},\n                    dir: {d},\n"
        f"                    nullable: {'true' if a.nullable else 'false'},\n"
        f"                    when: {rust_str(a.when)},\n                }})"
    )


def kernel_cast(a: Arg, reg: int) -> str:
    r = f"regs[{reg}]"
    return r if a.ty.kernel_rust == "u64" else f"{r} as {a.ty.kernel_rust}"


def kernel_params(row: Row) -> str:
    return "".join(f", {a.name}: {a.ty.kernel_rust}" for a in row.args)


def render_arch(table: Table, arch: str, rule: str, doc: str) -> list[str]:
    """`arch`'s numbers, `TABLE`, `call` and `dispatch` as an inline `pub mod`."""
    rows = [r for r in table.rows if r.nr(arch) is not None]
    size = max((r.nr(arch) or 0) for r in rows) + 1
    out = [
        f"/// {doc}",
        f"pub mod {arch} {{",
        "    use super::{Handlers, NrRule, NrTable, Sys, SysResult};",
        "    use crate::kerror::KError;",
        "",
        f"    /// The {arch} numbers.",
        "    pub mod nr {",
    ]
    for r in rows:
        out += [
            f"        /// `{r.name}`.",
            f"        pub const SYS_{r.name.upper()}: u64 = {r.nr(arch)};",
        ]
    out += [
        "    }",
        "",
        f"    const SLOTS: [Option<Sys>; {size}] = {{",
        f"        let mut t = [None; {size}];",
    ]
    for r in rows:
        out.append(f"        t[nr::SYS_{r.name.upper()} as usize] = Some(Sys::{r.variant});")
    out += [
        "        t",
        "    };",
        "",
        "    /// The table dispatch indexes by number.",
        f"    pub static TABLE: NrTable = NrTable::new(NrRule::{rule}, &SLOTS);",
        "",
        "    /// Call `sys`'s handler, each register cut to its argument's C type.",
        "    pub fn call<H: Handlers + ?Sized>(h: &mut H, sys: Sys, regs: &[u64; 6])"
        " -> SysResult {",
        "        match sys {",
    ]
    no_nr = [r for r in table.rows if r.nr(arch) is None]
    for r in table.rows:
        if r.nr(arch) is None:
            continue
        if arch == "aarch64":
            reg_of = {arg: reg for reg, arg in enumerate(r.aarch64_order)}
        else:
            reg_of = {i: i for i in range(len(r.args))}
        call_args = [kernel_cast(a, reg_of[i]) for i, a in enumerate(r.args)]
        out += rust_list("            ", f"Sys::{r.variant} => h.{r.name}(", call_args, ")", ",")
    if no_nr:
        pat = " | ".join(f"Sys::{r.variant}" for r in no_nr)
        out.append(f"            {pat} => Err(KError::NoSys),")
    out += [
        "        }",
        "    }",
        "",
        "    /// Look `raw_nr` up as Linux reads it and call its handler; a",
        "    /// number that names no row is `ENOSYS`.",
        "    pub fn dispatch<H: Handlers + ?Sized>(h: &mut H, raw_nr: u64, regs: &[u64; 6])"
        " -> SysResult {",
        "        match TABLE.lookup(raw_nr) {",
        "            Some(sys) => call(h, sys, regs),",
        "            None => Err(KError::NoSys),",
        "        }",
        "    }",
        "}",
    ]
    return out


def render_x86(table: Table) -> str:
    """x86_64's module as its own file in the port's pure half: `render_arch`'s
    module body, dedented, importing the shared types from the table."""
    body = render_arch(
        table,
        "x86_64",
        "SignExtendEax",
        "x86_64: the number is `eax` sign-extended (SYSCALL.md §1).",
    )[2:-1]
    body = [ln[4:] if ln.startswith("    ") else ln for ln in body]
    body[0:2] = [
        "use crate::kerror::KError;",
        "use crate::proc::syscall_table::{Handlers, NrRule, NrTable, Sys, SysResult};",
    ]
    out = [
        f"// {GENERATED}.",
        "// Do not edit: change the table and run the script (ROADMAP §10.5).",
        "",
        "//! The x86_64 syscall numbers, the table dispatch indexes, and the typed",
        "//! `call`/`dispatch` (SYSCALL.md §1, §3): the pure half of the `SyscallAbi`",
        "//! row (PORTABILITY §11.1). The number is `eax` sign-extended.",
        "",
    ]
    out += body
    out.append("")
    return "\n".join(out)


def val_variant(t: CType) -> str:
    return {
        "i32": "I32",
        "u32": "U32",
        "i64": "I64",
        "u64": "U64",
        "usize": "Usize",
        "u16": "U16",
    }[t.kernel_rust] if not t.is_ptr else "Ptr"


def render_kernel(table: Table) -> str:
    n = len(table.rows)
    out = [
        f"// {GENERATED}.",
        "// Do not edit: change the table and run the script (ROADMAP §10.5).",
        "",
        "//! The syscall table (ROADMAP §10.5, C-SYSTABLE), generated from",
        "//! `syscalls.toml`: one [`Row`] per syscall, the [`Handlers`] each",
        "//! syscall calls, and per architecture the numbers and the typed",
        "//! `dispatch` (SYSCALL.md §1, §3).",
        "",
        "use crate::kerror::KError;",
        "",
        KERNEL_TYPES,
        "/// One syscall, whatever its number on each architecture.",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub enum Sys {",
    ]
    for r in table.rows:
        out += [f"    /// `{r.name}`.", f"    {r.variant},"]
    out += [
        "}",
        "",
        "impl Sys {",
        "    /// Every syscall, in table order.",
        f"    pub const ALL: [Sys; {n}] = [",
    ]
    out += [f"        Sys::{r.variant}," for r in table.rows]
    out += [
        "    ];",
        "",
        "    /// This syscall's row.",
        "    pub const fn row(self) -> &'static Row {",
        "        &ROWS[self as usize]",
        "    }",
        "}",
        "",
        "/// The rows, in [`Sys`] order.",
        f"pub static ROWS: [Row; {n}] = [",
    ]
    for r in table.rows:
        out += [
            "    Row {",
            f"        sys: Sys::{r.variant},",
            f"        name: {rust_str(r.name)},",
        ]
        if len(r.args) == 1 and r.args[0].kind is None:
            # rustfmt's form for a one-element array of one struct.
            a = r.args[0]
            out += [
                "        args: &[Arg {",
                f"            name: {rust_str(a.name)},",
                f"            ty: CType::{a.ty.variant},",
                "            ptr: None,",
                "        }],",
            ]
        elif r.args:
            out.append("        args: &[")
            for a in r.args:
                out += [
                    "            Arg {",
                    f"                name: {rust_str(a.name)},",
                    f"                ty: CType::{a.ty.variant},",
                    f"                ptr: {kernel_ptr(a)},",
                    "            },",
                ]
            out.append("        ],")
        else:
            out.append("        args: &[],")
        out.append("    },")
    out += [
        "];",
        "",
        "/// The handlers: one method per row, each argument in its C type",
        "/// (SYSCALL.md §1). A new row fails to compile until its handler exists.",
        "pub trait Handlers {",
    ]
    for r in table.rows:
        out.append(f"    /// `{r.name}`.")
        sig = f"    fn {r.name}(&mut self{kernel_params(r)}) -> SysResult;"
        if len(sig) > 100:
            out.append(f"    fn {r.name}(")
            out.append("        &mut self,")
            out += [f"        {a.name}: {a.ty.kernel_rust}," for a in r.args]
            out.append("    ) -> SysResult;")
        else:
            out.append(sig)
    out += ["}", ""]
    out += render_arch(
        table,
        "aarch64",
        "Low32",
        "aarch64: the number is the low 32 bits of `x8`, unsigned (ROADMAP §11.6).",
    )
    out += [
        "",
        "/// One argument as a handler received it (host tests).",
        "#[cfg(test)]",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub enum Val {",
        "    I32(i32),",
        "    U32(u32),",
        "    I64(i64),",
        "    U64(u64),",
        "    Usize(usize),",
        "    U16(u16),",
        "    Ptr(u64),",
        "}",
        "",
        "/// A [`Handlers`] that keeps the last call's typed arguments (host tests).",
        "#[cfg(test)]",
        "#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]",
        "pub struct Recorder {",
        "    /// The last call.",
        "    pub sys: Option<Sys>,",
        "    /// Its arguments, in canonical order.",
        "    pub args: [Option<Val>; 6],",
        "}",
        "",
        "#[cfg(test)]",
        "impl Recorder {",
        "    fn record(&mut self, sys: Sys, vals: &[Val]) -> SysResult {",
        "        let mut args = [None; 6];",
        "        for (slot, v) in args.iter_mut().zip(vals) {",
        "            *slot = Some(*v);",
        "        }",
        "        *self = Recorder {",
        "            sys: Some(sys),",
        "            args,",
        "        };",
        "        Ok(0)",
        "    }",
        "}",
        "",
        "#[cfg(test)]",
        "impl Handlers for Recorder {",
    ]
    for i, r in enumerate(table.rows):
        if i:
            out.append("")
        sig = f"    fn {r.name}(&mut self{kernel_params(r)}) -> SysResult {{"
        if len(sig) > 100:
            out.append(f"    fn {r.name}(")
            out.append("        &mut self,")
            out += [f"        {a.name}: {a.ty.kernel_rust}," for a in r.args]
            out.append("    ) -> SysResult {")
        else:
            out.append(sig)
        vals = [f"Val::{val_variant(a.ty)}({a.name})" for a in r.args]
        arr = rust_list("", "&[", vals, "]", "")
        call = [f"Sys::{r.variant}", arr[0]]
        if len(arr) == 1 and len(rust_list("        ", "self.record(", call, ")", "")) == 1:
            out += rust_list("        ", "self.record(", call, ")", "")
        else:
            out += ["        self.record(", f"            Sys::{r.variant},"]
            out += rust_list("            ", "&[", vals, "]", ",")
            out.append("        )")
        out.append("    }")
    out += ["}", ""]
    return "\n".join(out)


# ------------------------------------------------------------------ user


def user_cast(a: Arg) -> str:
    if a.ty.is_ptr:
        return f"{a.name} as usize"
    if a.ty.kernel_rust == "usize":
        return a.name
    if a.ty.signed:
        return f"{a.name} as isize as usize"
    return f"{a.name} as usize"


def user_type(a: Arg) -> str:
    return a.ty.pointee if a.ty.is_ptr else a.ty.kernel_rust


def writes_through(a: Arg) -> bool:
    return a.kind in ("buf", "fixed") and a.dir == "out"


def user_safety(r: Row) -> list[str]:
    lines: list[str] = []
    for a in r.args:
        if not writes_through(a):
            continue
        if a.kind == "buf":
            size = f"up to `{r.args[a.len_from].name}` bytes"
        else:
            size = f"{a.size} bytes"
        null = ", unless it is null" if a.nullable else ""
        lines.append(
            f"The kernel writes {size} through `{a.name}`{null}: no live Rust reference"
            " may cover them."
        )
    if r.unsafe:
        lines.append(r.unsafe[:1].upper() + r.unsafe[1:] + ".")
    return lines


def wrap_doc(text: str, indent: str, width: int = 100) -> list[str]:
    words = text.split()
    out: list[str] = []
    cur = ""
    for w in words:
        cand = f"{cur} {w}" if cur else w
        if len(indent) + 4 + len(cand) > width and cur:
            out.append(f"{indent}/// {cur}")
            cur = w
        else:
            cur = cand
    if cur:
        out.append(f"{indent}/// {cur}")
    return out


def render_user(table: Table) -> str:
    rows = [r for r in table.rows if r.x86_64 is not None]
    out = [
        f"// {GENERATED}.",
        "// Do not edit: change the table and run the script (ROADMAP §10.5).",
        "",
        "//! x86_64 system call stubs (C-USERRT, C-SYSTABLE), generated from the",
        "//! kernel's syscall table. Each takes its arguments in the C types of",
        "//! Linux's prototype and returns `Ok` with the result or the `Errno`.",
        "//! A stub is an `unsafe fn` when the kernel writes through one of its",
        "//! pointers or the row says why.",
        "",
    ]
    if any(a.ty.is_ptr and "c_void" in a.ty.pointee for r in rows for a in r.args):
        out += ["use core::ffi::c_void;", ""]
    out += [
        "/// The raw calls, for a call with no stub.",
        "pub use super::{syscall0, syscall1, syscall2, syscall3, syscall4, syscall5, syscall6};",
        "use crate::sys::{Errno, result};",
        "",
        "/// The x86_64 numbers.",
        "pub mod nr {",
    ]
    for r in rows:
        out += [f"    /// `{r.name}`.", f"    pub const SYS_{r.name.upper()}: usize = {r.x86_64};"]
    out += [
        "}",
        "",
        "/// One system call.",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub enum Sys {",
    ]
    for r in rows:
        out += [f"    /// `{r.name}`.", f"    {r.variant},"]
    out += [
        "}",
        "",
        "impl Sys {",
        "    /// Every call, in table order.",
        f"    pub const ALL: [Sys; {len(rows)}] = [",
    ]
    out += [f"        Sys::{r.variant}," for r in rows]
    out += [
        "    ];",
        "",
        "    /// The call named `name`.",
        "    pub fn from_name(name: &[u8]) -> Option<Sys> {",
        "        match name {",
    ]
    out += [f'            b"{r.name}" => Some(Sys::{r.variant}),' for r in rows]
    out += [
        "            _ => None,",
        "        }",
        "    }",
        "",
        "    /// The call's name.",
        "    pub const fn name(self) -> &'static str {",
        "        match self {",
    ]
    out += [f'            Sys::{r.variant} => "{r.name}",' for r in rows]
    out += [
        "        }",
        "    }",
        "",
        "    /// The call's number.",
        "    pub const fn nr(self) -> usize {",
        "        match self {",
    ]
    out += [f"            Sys::{r.variant} => nr::SYS_{r.name.upper()}," for r in rows]
    out += [
        "        }",
        "    }",
        "",
        "    /// The call's argument names, in register order.",
        "    pub const fn args(self) -> &'static [&'static str] {",
        "        match self {",
    ]
    for r in rows:
        names = ", ".join(f'"{a.name}"' for a in r.args)
        out.append(f"            Sys::{r.variant} => &[{names}],")
    out += ["        }", "    }", "}"]
    for r in rows:
        safety = user_safety(r)
        params = ", ".join(f"{a.name}: {user_type(a)}" for a in r.args)
        proto = f"`{r.name}({', '.join(c_decl(a) for a in r.args)})`"
        out.append("")
        out += wrap_doc(f"{proto}{': ' + r.note if r.note else ''}.", "")
        if safety:
            out += ["///", "/// # Safety", "///"]
            for i, s in enumerate(safety):
                if i:
                    out.append("///")
                out += wrap_doc(s, "")
        qual = "pub unsafe fn" if safety else "pub fn"
        sig = f"{qual} {r.name}({params}) -> Result<usize, Errno> {{"
        if len(sig) > 100:
            out.append(f"{qual} {r.name}(")
            out += [f"    {a.name}: {user_type(a)}," for a in r.args]
            out.append(") -> Result<usize, Errno> {")
        else:
            out.append(sig)
        if safety:
            out.append(
                "    // SAFETY: the kernel's `syscall` convention, and this fn's `# Safety`"
            )
            out.append("    // contract for what the call writes, established here by its caller.")
        else:
            out.append(
                "    // SAFETY: the kernel's `syscall` convention; the kernel writes through"
            )
            out.append("    // none of its pointers, established here by the table row.")
        sc_args = [f"nr::SYS_{r.name.upper()}"] + [user_cast(a) for a in r.args]
        one = rust_list("    result(unsafe { ", f"syscall{len(r.args)}(", sc_args, ")", " })")
        if len(one) == 1:
            out += one
        else:
            out.append("    result(unsafe {")
            out += rust_list("        ", f"syscall{len(r.args)}(", sc_args, ")", "")
            out.append("    })")
        out.append("}")
    out.append("")
    return "\n".join(out)


# ------------------------------------------------------------------ docs


def md_ptr(r: Row, a: Arg) -> str:
    if a.kind == "unread":
        return f"`{a.name}`: {a.when}"
    what = {
        "buf": f"{a.dir}, `{r.args[a.len_from].name}` bytes" if a.kind == "buf" else "",
        "fixed": f"{a.dir}, {a.size} bytes",
        "cstr": "C string",
        "strvec": "C string vector",
    }[a.kind or ""]
    null = ", may be NULL" if a.nullable else ""
    return f"`{a.name}`: {what}{null}, {a.when}"


def c_decl(a: Arg) -> str:
    """`a` as its C declaration: `char *buf`, `size_t count`."""
    sep = "" if a.ty.is_ptr else " "
    return f"{a.ty.text}{sep}{a.name}"


def render_md(table: Table) -> str:
    out = [
        "| x86_64 | aarch64 | name | arity | arguments | pointer arguments | notes |",
        "|---:|---:|------|------:|-----------|-------------------|-------|",
    ]
    for r in table.rows:
        args = ", ".join(f"`{c_decl(a)}`" for a in r.args) or "—"
        ptrs = "; ".join(md_ptr(r, a) for a in r.args if a.kind is not None) or "—"
        cells = [
            "—" if r.x86_64 is None else str(r.x86_64),
            "—" if r.aarch64 is None else str(r.aarch64),
            f"`{r.name}`",
            str(len(r.args)),
            args,
            ptrs,
            r.note or "—",
        ]
        out.append("| " + " | ".join(cells) + " |")
    return "\n" + "\n".join(out) + "\n\n"


# -------------------------------------------------------------- registry


@dataclass(frozen=True)
class Emitter:
    """One output: a whole file, or the marked `block` in a document."""

    path: Path
    render: Callable[[Table], str]
    block: str | None = None


EMITTERS: list[Emitter] = [
    Emitter(KERNEL_OUT, render_kernel),
    Emitter(X86_OUT, render_x86),
    Emitter(USER_OUT, render_user),
    Emitter(SYSCALL_MD, render_md, "syscall-table"),
    Emitter(SYSCALL_MD, render_errno_md, "errno-table"),
    Emitter(USER_ERRNO_OUT, render_user_errno),
]


def splice(doc: str, block: str, body: str, path: Path) -> str:
    """`doc` with the text between `block`'s markers replaced by `body`."""
    begin = f"<!-- gen_syscalls: begin {block} -->"
    end = f"<!-- gen_syscalls: end {block} -->"
    b = doc.find(begin)
    e = doc.find(end)
    if b < 0 or e < 0 or e < b or doc.count(begin) != 1 or doc.count(end) != 1:
        raise TableError(f"{path}: missing or repeated markers for block {block!r}")
    start = b + len(begin)
    return doc[:start] + "\n" + body + doc[e:]


def generate(text: str, root: Path = ROOT) -> dict[Path, str]:
    """Every output's text, by path relative to `root`, from table `text`."""
    table = parse(text)
    table.errno = load_errno_table(root / KERROR)
    outputs: dict[Path, str] = {}
    for em in EMITTERS:
        body = em.render(table)
        if em.block is None:
            outputs[em.path] = body
        else:
            base = outputs.get(em.path)
            if base is None:
                base = (root / em.path).read_text(encoding="utf-8")
            outputs[em.path] = splice(base, em.block, body, em.path)
    return outputs


def main(argv: Sequence[str] | None = None, root: Path = ROOT) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0] if __doc__ else None)
    ap.add_argument("--check", action="store_true", help="write nothing; fail on drift")
    args = ap.parse_args(argv)
    try:
        outputs = generate((root / TABLE).read_text(encoding="utf-8"), root)
    except (TableError, OSError) as e:
        print(f"gen_syscalls: {TABLE}: {e}", file=sys.stderr)
        return 1
    stale = []
    for path, text in outputs.items():
        target = root / path
        old = target.read_text(encoding="utf-8") if target.exists() else None
        if old == text:
            continue
        stale.append(path)
        if not args.check:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text, encoding="utf-8")
    if args.check and stale:
        for path in stale:
            print(
                f"gen_syscalls: {path} differs from {TABLE}; run scripts/gen_syscalls.py",
                file=sys.stderr,
            )
        return 1
    verb = "ok" if args.check else f"wrote {len(stale)}"
    print(f"gen_syscalls: {verb} ({len(outputs)} outputs)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
