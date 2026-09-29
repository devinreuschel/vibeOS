#!/usr/bin/env python3
"""No two kernel modules use each other (ROADMAP §10.3, A4; DESIGN §1.1 constraint 6).

Builds the module graph of both crates, `src/main.rs` (`kernel:`) and
`crates/core/src/lib.rs` (`core:`), from `use` trees and paths:

- Nodes. One production file module is one node, named by crate and path
  (`kernel:log::serial::raw`, `core:sched::wait`). A directory's `mod.rs` is the
  directory's module. The crate roots are not nodes, and a reference that
  resolves to a root item is not an edge. Modules, file or inline, declared
  under `#[cfg(test)]` or `#[cfg(feature = "kernel_tests")]` (alone or inside
  `all(..)`, never inside `not(..)` or `any(..)`) are left out with everything
  in them; gated items inside a production module still count.
- `ONE_MODULE` names directory modules that are one node: a file whose parent
  directory module is listed, and that is not listed itself, belongs to its
  parent's node. It holds the Q5 splits of P10-S26 and nothing else.
- Edges. Each `use`-tree leaf and each path starting with `crate`, `$crate`,
  `self`, `super`, `vibeos` or a child module is an edge, after comments and
  literals (`asm!` text included) are stripped. A path resolves through `mod`
  declarations and the module aliases (`use`, `pub use`, `as`) of the crate
  roots and `mod.rs` files, to the deepest file module it reaches. Macros are
  not expanded.

Rules, each failure printed as a key and both `file:line`s:

- `two-way <A> <B>` (A < B): A and B use each other.
- `heap <A> -> <B>`: A's leaf is `heap`, `heap_init`, `pmm` or `pmm_init`, and
  B's leaf starts with `sched`, `thread`, `wait`, `cache` or `proc`, or B's path
  has an `fs` segment.
- `raw <A> -> <B>`: a kernel-crate edge from `kernel:log::serial::raw` to any
  module but the one the kernel root's `x86` alias names. `raw macro <name>`: an
  invocation there of a `macro_rules!` macro the kernel defines. `raw missing`:
  the raw module does not exist.

Cycles of three or more nodes print as `check_cycles: note:` lines and do not
fail. Prints `check_cycles: ok (<n> modules, <m> edges)`, or the failures on
stderr and exits 1. `--root DIR` checks the tree under DIR instead.
"""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

KERNEL_ROOT = Path("src") / "main.rs"
CORE_ROOT = Path("crates") / "core" / "src" / "lib.rs"

# Directory modules that are one node (the Q5 splits of P10-S26).
ONE_MODULE: frozenset[str] = frozenset({
    "core:fs",
    "core:fs::kernfs",
    "core:fs::vibefs",
    "core:fs::fat",
    "kernel:proc::proc_init",
})

RAW = "kernel:log::serial::raw"
RAW_ALIAS = "x86"
HEAP_LEAVES = frozenset({"heap", "heap_init", "pmm", "pmm_init"})
UPPER_PREFIXES = ("sched", "thread", "wait", "cache", "proc")

# Exact failure keys the tree may still produce while this branch breaks them.
KNOWN: list[str] = [
    "raw missing",
    "two-way core:arch::x86_64::trap core:trap",
    "two-way core:fs core:fs::kernfs",
    "two-way kernel:arch::x86_64::catch kernel:arch::x86_64::idt",
    "two-way kernel:arch::x86_64::cpu kernel:cell",
    "two-way kernel:arch::x86_64::cpu kernel:smp::per_cpu_init",
    "two-way kernel:arch::x86_64::idt kernel:proc::proc_init",
    "two-way kernel:arch::x86_64::idt kernel:proc::syscall_init",
    "two-way kernel:block::block_init kernel:block::cache_init",
    "two-way kernel:block::block_init kernel:block::part_init",
    "two-way kernel:block::block_init kernel:drivers::virtio_blk_init",
    "two-way kernel:boot kernel:mm::paging_init",
    "two-way kernel:cell kernel:smp::per_cpu_init",
    "two-way kernel:dev::dev_init kernel:dev::pci_init",
    "two-way kernel:fs::fat_init kernel:fs::fs_init",
    "two-way kernel:fs::file_init kernel:fs::fs_init",
    "two-way kernel:fs::file_init kernel:fs::vibefs_init",
    "two-way kernel:fs::file_init kernel:shell::shell_init",
    "two-way kernel:fs::fs_init kernel:fs::vibefs_init",
    "two-way kernel:irq::ipi_init kernel:sched::thread_init",
    "two-way kernel:log::log_init kernel:log::serial",
    "two-way kernel:mm::paging_init kernel:mm::pmm_init",
    "two-way kernel:proc::proc_init kernel:proc::syscall_init",
    "two-way kernel:proc::syscall_init kernel:sched::thread_init",
    "two-way kernel:sched::thread_init kernel:sched::work_init",
    "two-way kernel:sched::thread_init kernel:sync::sync_init",
]

PATH_STARTS = frozenset({"crate", "$crate", "self", "super", "vibeos"})


# ---------------------------------------------------------------- tokens


@dataclass(frozen=True)
class Tok:
    kind: str  # "id", "str", "p" (punctuation)
    text: str
    line: int


def tokenize(src: str) -> list[Tok]:
    """Identifiers, string literals and punctuation, with comments dropped."""
    toks: list[Tok] = []
    i, n, line = 0, len(src), 1

    def ident_char(c: str) -> bool:
        return c.isalnum() or c == "_"

    while i < n:
        c = src[i]
        if c == "\n":
            line += 1
            i += 1
            continue
        if c.isspace():
            i += 1
            continue
        if src.startswith("//", i):
            j = src.find("\n", i)
            i = n if j < 0 else j
            continue
        if src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth += 1
                    j += 2
                elif src.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            line += src.count("\n", i, j)
            i = j
            continue
        # Raw strings: r"..", r#".."#, br"..", cr"..".
        k = i
        if src[k] in "bc" and k + 1 < n and src[k + 1] == "r":
            k += 1
        if src[k] == "r" and k + 1 < n and src[k + 1] in "#\"":
            j = k + 1
            hashes = 0
            while j < n and src[j] == "#":
                hashes += 1
                j += 1
            if j < n and src[j] == '"':
                close = '"' + "#" * hashes
                end = src.find(close, j + 1)
                end = n if end < 0 else end + len(close)
                toks.append(Tok("str", src[j + 1:end - len(close)], line))
                line += src.count("\n", i, end)
                i = end
                continue
        # Plain and byte strings.
        k = i
        if src[k] in "bc" and k + 1 < n and src[k + 1] == '"':
            k += 1
        if src[k] == '"':
            j = k + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            toks.append(Tok("str", src[k + 1:j], line))
            line += src.count("\n", i, j + 1)
            i = j + 1
            continue
        # Char and byte literals versus lifetimes.
        k = i + 1 if src[i] == "b" and i + 1 < n and src[i + 1] == "'" else i
        if src[k] == "'":
            if k + 1 < n and src[k + 1] == "\\":
                j = src.find("'", k + 2)
                if src[k + 2:k + 3] == "'":
                    j = src.find("'", k + 3)
                i = n if j < 0 else j + 1
                continue
            if k + 2 < n and src[k + 2] == "'":
                i = k + 3
                continue
            if k == i:
                # A lifetime or label: skip the quote, the name follows.
                j = k + 1
                while j < n and ident_char(src[j]):
                    j += 1
                i = j
                continue
        if ident_char(c):
            j = i
            while j < n and ident_char(src[j]):
                j += 1
            word = src[i:j]
            if word == "r" and j < n and src[j] == "#":
                k = j + 1
                while k < n and ident_char(src[k]):
                    k += 1
                word = src[j + 1:k]
                j = k
            if not word[0].isdigit():
                toks.append(Tok("id", word, line))
            i = j
            continue
        if src.startswith("::", i):
            toks.append(Tok("p", "::", line))
            i += 2
            continue
        if c == "$" and i + 1 < n and src.startswith("crate", i + 1) and not (
                i + 6 < n and ident_char(src[i + 6])):
            toks.append(Tok("id", "$crate", line))
            i += 6
            continue
        toks.append(Tok("p", c, line))
        i += 1
    return toks


def match_close(toks: list[Tok], i: int) -> int:
    """Index of the bracket closing the one at `i`."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[toks[i].text]]
    j = i + 1
    while j < len(toks) and stack:
        t = toks[j]
        if t.kind == "p":
            if t.text in pairs:
                stack.append(pairs[t.text])
            elif t.text == stack[-1]:
                stack.pop()
        j += 1
    return j - 1


# ---------------------------------------------------------------- cfg


def cfg_gated(toks: list[Tok]) -> bool:
    """Whether attribute tokens (inside `#[..]`) gate on test or kernel_tests."""
    if len(toks) < 3 or toks[0].text != "cfg" or toks[1].text != "(":
        return False
    pred, _ = _cfg_pred(toks, 2)
    return pred


def _cfg_pred(toks: list[Tok], i: int) -> tuple[bool, int]:
    """Parse one predicate at `i`; return (gated, index after it)."""
    if i >= len(toks):
        return False, i
    t = toks[i]
    if t.kind == "id" and i + 1 < len(toks) and toks[i + 1].text == "(":
        end = match_close(toks, i + 1)
        subs: list[bool] = []
        j = i + 2
        while j < end:
            g, j = _cfg_pred(toks, j)
            subs.append(g)
            if j < end and toks[j].text == ",":
                j += 1
        if t.text == "all":
            return any(subs), end + 1
        return False, end + 1
    if t.kind == "id" and i + 2 < len(toks) and toks[i + 1].text == "=":
        return (t.text == "feature" and toks[i + 2].text == "kernel_tests"), i + 3
    if t.kind == "id":
        return t.text == "test", i + 1
    return False, i + 1


# ---------------------------------------------------------------- modules


@dataclass
class Module:
    crate: str
    path: tuple[str, ...]
    file: Path | None  # file of a file module; None for an inline module
    host: Module | None  # the file module an inline module sits in
    parent: Module | None
    is_dir: bool  # a crate root or a mod.rs: its aliases resolve
    children: dict[str, Module] = field(default_factory=dict)
    aliases: dict[str, tuple[list[str], int]] = field(default_factory=dict)

    @property
    def name(self) -> str:
        return f"{self.crate}:" + "::".join(self.path)

    def file_module(self) -> Module:
        m = self
        while m.file is None and m.host is not None:
            m = m.host
        return m


@dataclass(frozen=True)
class Ref:
    scope: Module
    segs: tuple[str, ...]
    line: int


@dataclass
class Crate:
    name: str
    root: Module
    modules: list[Module] = field(default_factory=list)
    refs: list[Ref] = field(default_factory=list)
    macros: set[str] = field(default_factory=set)
    # (file module, macro name, line) for every `name!` invocation.
    invocations: list[tuple[Module, str, int]] = field(default_factory=list)


def _use_leaves(toks: list[Tok], i: int, prefix: list[str],
                out: list[tuple[list[str], str | None, int]]) -> int:
    """Parse a use tree at `i`. Append (segments, bound name, line) per leaf."""
    segs = list(prefix)
    while i < len(toks):
        t = toks[i]
        if t.text == "{":
            end = match_close(toks, i)
            j = i + 1
            while j < end:
                j = _use_leaves(toks, j, segs, out)
                if j < end and toks[j].text == ",":
                    j += 1
            return end + 1
        if t.text == "*":
            out.append((segs, None, t.line))
            return i + 1
        if t.kind == "id":
            if t.text == "self" and segs and segs == prefix and i > 0 and toks[i - 1].text in "{,":
                name: str | None = segs[-1]
                full = segs
            else:
                full = segs + [t.text]
                name = t.text
            i += 1
            if i + 1 < len(toks) and toks[i].text == "as":
                name = toks[i + 1].text
                i += 2
            if i < len(toks) and toks[i].text == "::":
                segs = full
                i += 1
                continue
            out.append((full, None if name == "_" else name, t.line))
            return i
        if t.text == "::":
            i += 1
            continue
        return i + 1
    return i


def _mod_file(owner: Module, name: str, path_attr: str | None) -> Path | None:
    """File that `mod name;` in `owner` loads, or None."""
    host = owner.file_module()
    assert host.file is not None
    base = host.file.parent
    if not host.is_dir:
        base = base / host.file.stem
    inline: list[str] = []
    m = owner
    while m.file is None and m.host is not None:
        inline.append(m.path[-1])
        m = m.parent if m.parent is not None else m.host
    for seg in reversed(inline):
        base = base / seg
    if path_attr is not None:
        p = (host.file.parent if not inline else base) / path_attr
        return p if p.is_file() else None
    for cand in (base / f"{name}.rs", base / name / "mod.rs"):
        if cand.is_file():
            return cand
    return None


def parse_file(crate: Crate, mod: Module, root_dir: Path) -> None:
    """Scan one file module: declare children, collect refs and aliases."""
    assert mod.file is not None
    toks = tokenize(mod.file.read_text(encoding="utf-8", errors="replace"))
    local = {toks[k + 1].text for k in range(len(toks) - 1)
             if toks[k].text == "mod" and toks[k + 1].kind == "id"}
    _parse_block(crate, mod, toks, 0, len(toks), root_dir, local, file_start=True)


def _parse_block(crate: Crate, scope: Module, toks: list[Tok], i: int, end: int,
                 root_dir: Path, local: set[str], file_start: bool = False) -> None:
    pending: list[list[Tok]] = []
    prev = ""
    while i < end:
        t = toks[i]
        if t.text == "#" and i + 1 < end and toks[i + 1].text in ("[", "!"):
            inner = toks[i + 1].text == "!"
            open_i = i + 2 if inner else i + 1
            if open_i >= end or toks[open_i].text != "[":
                i += 1
                continue
            close = match_close(toks, open_i)
            body = toks[open_i + 1:close]
            if inner:
                if file_start and cfg_gated(body):
                    scope.children.clear()
                    return
            else:
                pending.append(body)
            i = close + 1
            prev = "]"
            continue
        if t.kind == "id" and t.text == "pub":
            i += 1
            if i < end and toks[i].text == "(":
                i = match_close(toks, i) + 1
            continue
        file_start = False
        if t.kind == "id" and t.text == "mod" and prev not in ("::", ".") and i + 1 < end \
                and toks[i + 1].kind == "id":
            name = toks[i + 1].text
            gated = any(cfg_gated(a) for a in pending)
            path_attr = None
            for a in pending:
                if len(a) >= 3 and a[0].text == "path" and a[1].text == "=":
                    path_attr = a[2].text
            pending = []
            nxt = toks[i + 2] if i + 2 < end else None
            if nxt is not None and nxt.text == "{":
                close = match_close(toks, i + 2)
                if not gated:
                    child = Module(scope.crate, scope.path + (name,), None,
                                   scope.file_module(), scope, False)
                    scope.children[name] = child
                    _parse_block(crate, child, toks, i + 3, close, root_dir, local)
                i = close + 1
                prev = "}"
                continue
            if not gated:
                f = _mod_file(scope, name, path_attr)
                if f is not None:
                    child = Module(scope.crate, scope.path + (name,), f, None, scope,
                                   f.name == "mod.rs")
                    scope.children[name] = child
                    crate.modules.append(child)
                    parse_file(crate, child, root_dir)
            i += 2
            prev = name
            continue
        if t.kind == "id" and t.text == "use" and prev not in ("::", "."):
            pending = []
            leaves: list[tuple[list[str], str | None, int]] = []
            j = i + 1
            if j < end and toks[j].text == "::":
                j += 1
            j = _use_leaves(toks, j, [], leaves)
            for segs, bound, line in leaves:
                if not segs:
                    continue
                crate.refs.append(Ref(scope, tuple(segs), line))
                if bound is not None and scope.is_dir:
                    scope.aliases.setdefault(bound, (segs, line))
            while j < end and toks[j].text != ";":
                j += 1
            i = j + 1
            prev = ";"
            continue
        if t.kind == "id" and t.text == "macro_rules" and i + 2 < end and toks[i + 1].text == "!":
            crate.macros.add(toks[i + 2].text)
        if t.kind == "id" and i + 1 < end and toks[i + 1].text == "!" and prev != "macro_rules" \
                and i + 2 < end and toks[i + 2].text in ("(", "[", "{"):
            crate.invocations.append((scope.file_module(), t.text, t.line))
        if t.kind == "id" and prev not in ("::", ".") and i + 1 < end and toks[i + 1].text == "::" \
                and (t.text in PATH_STARTS or t.text in local):
            segs = [t.text]
            j = i + 1
            while j + 1 < end and toks[j].text == "::" and toks[j + 1].kind == "id":
                segs.append(toks[j + 1].text)
                j += 2
            crate.refs.append(Ref(scope, tuple(segs), t.line))
            if j + 1 < end and toks[j].text == "!" and toks[j + 1].text in ("(", "[", "{"):
                crate.invocations.append((scope.file_module(), segs[-1], t.line))
            pending = []
            i = j
            prev = segs[-1]
            continue
        pending = []
        prev = t.text
        i += 1


def load_crate(name: str, root_file: Path, root_dir: Path) -> Crate:
    root = Module(name, (), root_file, None, None, True)
    crate = Crate(name, root)
    if root_file.is_file():
        parse_file(crate, root, root_dir)
    return crate


# ---------------------------------------------------------------- resolve


class Resolver:
    def __init__(self, kernel: Crate, core: Crate) -> None:
        self.kernel = kernel
        self.core = core

    def crate_root(self, scope: Module) -> Module:
        return self.kernel.root if scope.crate == "kernel" else self.core.root

    def start(self, scope: Module, seg: str) -> Module | None:
        if seg in ("crate", "$crate"):
            return self.crate_root(scope)
        if seg == "vibeos":
            return self.core.root
        if seg == "self":
            return scope
        if seg == "super":
            return scope.parent
        return self.lookup(scope, seg, 0)

    def lookup(self, m: Module, seg: str, depth: int) -> Module | None:
        if seg in m.children:
            return m.children[seg]
        if m.is_dir and seg in m.aliases and depth < 16:
            segs, _ = m.aliases[seg]
            return self.resolve(m, tuple(segs), depth + 1)
        return None

    def resolve(self, scope: Module, segs: tuple[str, ...], depth: int = 0) -> Module | None:
        """Deepest module a path reaches, or None when it leaves the crates."""
        if not segs:
            return None
        cur = self.start(scope, segs[0])
        if cur is None:
            return None
        for seg in segs[1:]:
            if seg == "super":
                nxt = cur.parent
            elif seg == "self":
                nxt = cur
            else:
                nxt = self.lookup(cur, seg, depth)
            if nxt is None:
                break
            cur = nxt
        return cur


def node_of(m: Module) -> str | None:
    """Graph node of a module, or None for a crate root."""
    f = m.file_module()
    if not f.path:
        return None
    name = f.name
    parent = f"{f.crate}:" + "::".join(f.path[:-1]) if len(f.path) > 1 else None
    if parent is not None and parent in ONE_MODULE and name not in ONE_MODULE:
        return parent
    return name


# ---------------------------------------------------------------- graph


@dataclass
class Graph:
    nodes: dict[str, str] = field(default_factory=dict)  # node -> file (repo-relative)
    edges: dict[tuple[str, str], str] = field(default_factory=dict)  # -> "file:line"


def build(root_dir: Path) -> tuple[Graph, Crate, Crate, Resolver]:
    kernel = load_crate("kernel", root_dir / KERNEL_ROOT, root_dir)
    core = load_crate("core", root_dir / CORE_ROOT, root_dir)
    res = Resolver(kernel, core)
    g = Graph()
    for crate in (kernel, core):
        for m in crate.modules:
            n = node_of(m)
            if n is not None and m.file is not None:
                if n not in g.nodes or n == m.name:
                    g.nodes[n] = rel(m.file, root_dir)
        for r in crate.refs:
            src = node_of(r.scope)
            if src is None:
                continue
            tgt = res.resolve(r.scope, r.segs)
            if tgt is None:
                continue
            dst = node_of(tgt)
            if dst is None or dst == src:
                continue
            f = r.scope.file_module().file
            assert f is not None
            g.edges.setdefault((src, dst), f"{rel(f, root_dir)}:{r.line}")
    return g, kernel, core, res


def rel(p: Path, root_dir: Path) -> str:
    try:
        return str(p.relative_to(root_dir))
    except ValueError:
        return str(p)


def leaf(node: str) -> str:
    return node.split(":", 1)[1].split("::")[-1]


def segments(node: str) -> list[str]:
    return node.split(":", 1)[1].split("::")


def failures(g: Graph, kernel: Crate, res: Resolver) -> dict[str, str]:
    """Failure key -> the `file:line` evidence."""
    out: dict[str, str] = {}
    for (a, b), where in g.edges.items():
        if (b, a) in g.edges and a < b:
            out[f"two-way {a} {b}"] = f"{where} ; {g.edges[(b, a)]}"
        if leaf(a) in HEAP_LEAVES and (leaf(b).startswith(UPPER_PREFIXES) or "fs" in segments(b)):
            out[f"heap {a} -> {b}"] = f"{where} ; {g.nodes.get(b, '?')}:1"
    raw_ok: str | None = None
    alias = kernel.root.aliases.get(RAW_ALIAS)
    if alias is not None:
        m = res.resolve(kernel.root, tuple(alias[0]))
        raw_ok = node_of(m) if m is not None else None
    if RAW not in g.nodes:
        out["raw missing"] = f"{KERNEL_ROOT}:1"
    else:
        for (a, b), where in g.edges.items():
            if a == RAW and b.startswith("kernel:") and b != raw_ok:
                out[f"raw {a} -> {b}"] = f"{where} ; {g.nodes.get(b, '?')}:1"
        for m, name, line in kernel.invocations:
            if node_of(m) == RAW and name in kernel.macros:
                out.setdefault(f"raw macro {name}", f"{g.nodes[RAW]}:{line}")
    return out


def long_cycles(g: Graph) -> list[list[str]]:
    """Strongly connected components of three or more nodes (Tarjan)."""
    adj: dict[str, list[str]] = {n: [] for n in g.nodes}
    for a, b in g.edges:
        adj.setdefault(a, []).append(b)
        adj.setdefault(b, [])
    index: dict[str, int] = {}
    low: dict[str, int] = {}
    on: set[str] = set()
    stack: list[str] = []
    comps: list[list[str]] = []
    counter = [0]

    def visit(v: str) -> None:
        work = [(v, iter(sorted(adj[v])))]
        index[v] = low[v] = counter[0]
        counter[0] += 1
        stack.append(v)
        on.add(v)
        while work:
            node, it = work[-1]
            advanced = False
            for w in it:
                if w not in index:
                    index[w] = low[w] = counter[0]
                    counter[0] += 1
                    stack.append(w)
                    on.add(w)
                    work.append((w, iter(sorted(adj[w]))))
                    advanced = True
                    break
                if w in on:
                    low[node] = min(low[node], index[w])
            if advanced:
                continue
            work.pop()
            if work:
                low[work[-1][0]] = min(low[work[-1][0]], low[node])
            if low[node] == index[node]:
                comp: list[str] = []
                while True:
                    w = stack.pop()
                    on.discard(w)
                    comp.append(w)
                    if w == node:
                        break
                if len(comp) >= 3:
                    comps.append(sorted(comp))

    for v in sorted(adj):
        if v not in index:
            visit(v)
    return sorted(comps)


def check(root_dir: Path, known: list[str]) -> tuple[list[str], list[str], Graph]:
    """(errors, notes, graph) for the tree under `root_dir`."""
    g, kernel, _core, res = build(root_dir)
    fails = failures(g, kernel, res)
    errors: list[str] = []
    notes: list[str] = []
    for key in sorted(fails):
        if key in known:
            notes.append(f"known {key}: {fails[key]}")
        else:
            errors.append(f"{key}: {fails[key]}")
    for key in known:
        if key not in fails:
            errors.append(f"KNOWN: stale key {key!r}; delete it")
    for comp in long_cycles(g):
        notes.append("cycle of " + str(len(comp)) + ": " + " ".join(comp))
    return errors, notes, g


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", type=Path, default=ROOT, help="tree to check")
    ap.add_argument("--edges", action="store_true", help="print every edge")
    args = ap.parse_args(argv)
    errors, notes, g = check(args.root.resolve(), KNOWN)
    if args.edges:
        for (a, b), where in sorted(g.edges.items()):
            print(f"{a} -> {b}  {where}")
    for n in notes:
        print(f"check_cycles: note: {n}")
    if errors:
        for e in errors:
            print(f"check_cycles: {e}", file=sys.stderr)
        print("check_cycles: break each pair with DESIGN §1.2's rules (a hook, a moved item, "
              "or a pull), never by merging modules", file=sys.stderr)
        return 1
    print(f"check_cycles: ok ({len(g.nodes)} modules, {len(g.edges)} edges)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
