#!/usr/bin/env python3
"""Every `// SAFETY:` comment names its invariant and where it is established.

AGENTS.md rule 7 (ROADMAP §10.1, F041): a SAFETY comment names the invariant,
as `invariant I<n>` when DESIGN §2.7 (docs/INVARIANTS.md) has a row for it,
and where it is established, as the module path of the function, method,
type, static, or const that establishes it, or `here` when the enclosing
function does. This script checks that form:

- A comment (the `// SAFETY:` line and the `//` lines after it) in `src/`,
  `crates/` or `user/` fails unless it says `here`, or names a `::` path
  that resolves from its crate root to a module defining the last segment:
  a `fn`, a type, a `static` or a `const`, or a method or associated item in
  an `impl` of the segment before it. `crate::`, `self::` and `super::`
  work, and `vibeos::` leads from the kernel into `vibeos-core`.
- Each `invariant I<n>` it names needs an `| I<n> |` row in
  docs/INVARIANTS.md.

The resolver walks `mod` lines (with `#[path]`) from the crate roots and
follows `use` and `pub use` aliases. It does not resolve inline modules or
re-exported items.

`PENDING` lists the files that fail at this tip; they are skipped until the
sweep that owns each fixes it and removes its entry, and an entry that now
passes is an error. `STALE_CLAIMS` holds wordings that were false against
the code (ROADMAP §10.1's four safety comments); each fails when it
reappears.

    check_safety.py [--failing]

`--failing` prints the files that fail, `PENDING` ignored, one per line.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Crate roots: (name used in paths, root file). `vibeos` is `vibeos-core`.
CRATES: tuple[tuple[str, str], ...] = (
    ("kernel", "src/main.rs"),
    ("vibeos", "crates/core/src/lib.rs"),
)
SCOPE = ("src", "crates", "user")
INVARIANTS = "docs/INVARIANTS.md"

# Files whose SAFETY comments fail at this tip, skipped until their sweep
# (ROADMAP §10.1, C-LINTS) fixes them; `--failing` regenerates the list.
PENDING: tuple[str, ...] = (
    "crates/core/src/kalloc.rs",
    "crates/core/src/mm/pmm.rs",
    "crates/core/src/proc/addr_space/mod.rs",
    "crates/core/src/smp/per_cpu.rs",
    "src/arch/ktest.rs",
    "src/arch/x86_64/idt.rs",
    "src/log/log_init.rs",
    "src/log/serial/raw.rs",
    "src/proc/addr_space_init.rs",
    "src/sched/thread_init.rs",
)

# (path, regex of a false wording) for statements corrected against the code
# (ROADMAP §10.1, F041). A match fails.
STALE_CLAIMS: tuple[tuple[str, str], ...] = (
    # `try_current` reads `gs:[0]` once `LIVE` is set.
    ("src/smp/per_cpu_init.rs", r"No-op before \[`init_bsp`\], or if GS is still 0"),
    # Software writes `TSS.RSP0` (`syscall_init::set_rsp0_for`).
    ("src/arch/x86_64/gdt.rs", r"Hardware updates RSP0"),
    # `Mapper` is auto-`Sync`.
    ("crates/core/src/mm/paging.rs", r"Not `Sync`: the design uses one root"),
    # `IrqCell` also guards cross-CPU data through its owner CAS.
    ("src/cell.rs", r"\[`IrqCell`\]: CPU-local or boot-only mutable"),
)

IDENT = r"[A-Za-z_][A-Za-z0-9_]*"
PATH = re.compile(rf"(?<![A-Za-z0-9_:]){IDENT}(?:::{IDENT})+")
HERE = re.compile(r"\bhere\b")
INVARIANT_IDS = re.compile(r"\binvariants?\s+(I\d+(?:(?:\s*,\s*|\s+and\s+|\s+or\s+)I\d+)*)")
ID = re.compile(r"I\d+")
REGISTER_ROW = re.compile(r"^\|\s*(I\d+)\s*\|", re.M)

MOD_DECL = re.compile(rf"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+({IDENT})\s*;")
PATH_ATTR = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\]')
DEFN = re.compile(
    rf"^\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|unsafe|async|extern\s+\"[^\"]*\"|default)\s+)*"
    rf"(?:fn|struct|enum|union|type|trait|static(?:\s+mut)?|const|macro_rules!)\s+({IDENT})")
IMPL = re.compile(
    rf"^\s*(?:unsafe\s+)?impl\b(?:\s*<[^{{]*?>)?\s+(?:[^{{]*?\bfor\s+)?(?:&(?:'\w+\s+)?(?:mut\s+)?)?"
    rf"(?:{IDENT}::)*({IDENT})")
USE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+([^;]+);", re.M)


@dataclass
class Module:
    crate: str
    path: tuple[str, ...]
    file: Path | None
    children: dict[str, tuple[str, ...]] = field(default_factory=dict)
    aliases: dict[str, tuple[str, ...]] = field(default_factory=dict)
    defs: set[str] = field(default_factory=set)
    impl_items: dict[str, set[str]] = field(default_factory=dict)


def strip_comments(text: str) -> str:
    """`text` with `//` comments and string contents blanked, lines kept."""
    out = []
    for line in text.splitlines():
        i = line.find("//")
        out.append(line if i < 0 else line[:i])
    return "\n".join(out)


def expand_use(tree: str) -> list[tuple[str, tuple[str, ...]]]:
    """(local name, path) for each leaf of a `use` tree; globs skipped."""
    tree = re.sub(r"\s+", "", re.sub(r"\s+as\s+", "@", tree.strip()))
    out: list[tuple[str, tuple[str, ...]]] = []

    def walk(prefix: tuple[str, ...], t: str) -> None:
        if "{" in t and t.endswith("}"):
            head, inner = t[: t.index("{")], t[t.index("{") + 1: -1]
            base = prefix + tuple(s for s in head.split("::") if s)
            depth, start = 0, 0
            for i, ch in enumerate(inner + ","):
                if ch == "{":
                    depth += 1
                elif ch == "}":
                    depth -= 1
                elif ch == "," and depth == 0:
                    if inner[start:i]:
                        walk(base, inner[start:i])
                    start = i + 1
            return
        leaf, _, rename = t.partition("@")
        full = prefix + tuple(s for s in leaf.split("::") if s)
        if full and full[-1] == "self":
            full = full[:-1]
        if not full or full[-1] == "*":
            return
        out.append((rename or full[-1], full))

    walk((), tree)
    return out


class ModuleIndex:
    """Modules of each crate, from its root's `mod` lines, with their items."""

    def __init__(self, root: Path = ROOT, crates: Iterable[tuple[str, str]] = CRATES) -> None:
        self.root = root
        self.mods: dict[tuple[str, tuple[str, ...]], Module] = {}
        self.by_file: dict[Path, Module] = {}
        for crate, rel in crates:
            f = root / rel
            if f.is_file():
                self._load(crate, (), f)

    def _load(self, crate: str, path: tuple[str, ...], f: Path) -> None:
        if (crate, path) in self.mods or f.resolve() in self.by_file:
            return
        text = f.read_text(encoding="utf-8", errors="replace")
        code = strip_comments(text)
        mod = Module(crate, path, f)
        self.mods[(crate, path)] = mod
        self.by_file[f.resolve()] = mod
        lines = code.splitlines()
        for i, line in enumerate(lines):
            m = MOD_DECL.match(line)
            if not m:
                continue
            name = m.group(1)
            child = None
            j = i - 1
            while j >= 0 and lines[j].strip().startswith("#"):
                pm = PATH_ATTR.match(lines[j])
                if pm:
                    child = (f.parent / pm.group(1))
                j -= 1
            if child is None:
                base = f.parent if f.name in ("main.rs", "lib.rs", "mod.rs") else f.parent / f.stem
                for cand in (base / f"{name}.rs", base / name / "mod.rs"):
                    if cand.is_file():
                        child = cand
                        break
            mod.children[name] = path + (name,)
            if child is not None and child.is_file():
                self._load(crate, path + (name,), child)
        for m in USE.finditer(code):
            for local, full in expand_use(m.group(1)):
                mod.aliases[local] = full
        self._items(mod, lines)

    @staticmethod
    def _items(mod: Module, lines: list[str]) -> None:
        depth = 0
        impl_stack: list[tuple[int, str]] = []
        for line in lines:
            im = IMPL.match(line)
            if im and "{" in line[im.end():] + line:
                impl_stack.append((depth, im.group(1)))
            dm = DEFN.match(line)
            if dm:
                if impl_stack and depth == impl_stack[-1][0] + 1:
                    mod.impl_items.setdefault(impl_stack[-1][1], set()).add(dm.group(1))
                else:
                    mod.defs.add(dm.group(1))
            depth += line.count("{") - line.count("}")
            while impl_stack and depth <= impl_stack[-1][0]:
                impl_stack.pop()

    def module_of(self, f: Path) -> Module | None:
        return self.by_file.get(f.resolve())

    def _start(self, mod: Module, seg: str) -> tuple[str, tuple[str, ...]] | None:
        if seg == "crate":
            return (mod.crate, ())
        if seg == "self":
            return (mod.crate, mod.path)
        if seg == "super":
            return (mod.crate, mod.path[:-1]) if mod.path else None
        if seg == "vibeos":
            return ("vibeos", ())
        return None

    def _lookup(self, crate: str, path: tuple[str, ...], seg: str,
                seen: frozenset[tuple[str, tuple[str, ...], str]] = frozenset()
                ) -> tuple[str, str, tuple[str, ...]] | None:
        """What `seg` names in module `path`: ("mod", crate, path) or ("item", crate, path+seg)."""
        key = (crate, path, seg)
        if key in seen:
            return None
        seen = seen | {key}
        mod = self.mods.get((crate, path))
        if mod is None:
            return None
        if seg in mod.children and (crate, mod.children[seg]) in self.mods:
            return ("mod", crate, mod.children[seg])
        if seg in mod.aliases:
            target = self._walk_use(mod, mod.aliases[seg], seen)
            if target is not None:
                return target
        if seg in mod.defs or seg in mod.impl_items:
            return ("item", crate, path + (seg,))
        return None

    def _walk_use(self, mod: Module, full: tuple[str, ...],
                  seen: frozenset[tuple[str, tuple[str, ...], str]]
                  ) -> tuple[str, str, tuple[str, ...]] | None:
        start = self._start(mod, full[0])
        rest = full[1:]
        if start is None:
            # 2018 paths: a name in this module, else an extern crate.
            start = (mod.crate, mod.path)
            rest = full
            if full[0] not in mod.children and full[0] not in mod.aliases:
                if full[0] == "vibeos":
                    start, rest = ("vibeos", ()), full[1:]
                else:
                    start = (mod.crate, ())
        crate, path = start
        target: tuple[str, str, tuple[str, ...]] = ("mod", crate, path)
        for seg in rest:
            while seg == "super" and target[0] == "mod":
                target = ("mod", target[1], target[2][:-1])
                seg = ""
            if not seg:
                continue
            if target[0] != "mod":
                return None
            nxt = self._lookup(target[1], target[2], seg, seen)
            if nxt is None:
                return None
            target = nxt
        return target


def resolves(index: ModuleIndex, mod: Module, path: str) -> bool:
    """Whether `a::b::c` resolves from `mod`'s crate root (or from `crate`,
    `self`, `super` or `vibeos`) to a module defining `c`, or to a type `b`
    with an impl item `c`."""
    segs = path.split("::")
    start = index._start(mod, segs[0])
    cur: tuple[str, ...]
    if start is None:
        crate, cur = mod.crate, ()
    else:
        (crate, cur), segs = start, segs[1:]
        while segs and segs[0] == "super":
            if not cur:
                return False
            cur, segs = cur[:-1], segs[1:]
    for i, seg in enumerate(segs):
        found = index._lookup(crate, cur, seg)
        left = len(segs) - i
        if left == 1:
            return found is not None and found[0] == "item"
        if left == 2 and found is not None and found[0] == "item":
            return _impl_has(index, found[1], found[2][-1], segs[-1])
        if found is None or found[0] != "mod":
            return False
        crate, cur = found[1], found[2]
    return False


def _impl_has(index: ModuleIndex, crate: str, tyname: str, item: str) -> bool:
    """Whether any module of `crate` has an `impl` of `tyname` holding `item`."""
    return any(item in m.impl_items.get(tyname, ())
               for (c, _), m in index.mods.items() if c == crate)


@dataclass
class Comment:
    file: Path
    line: int
    text: str


def safety_comments(text: str, file: Path = Path("?")) -> list[Comment]:
    """Each `// SAFETY:` comment in `text`, with the `//` lines that follow it."""
    lines = text.splitlines()
    out: list[Comment] = []
    i = 0
    while i < len(lines):
        s = lines[i].strip()
        if s.startswith("// SAFETY:"):
            start = i
            body = [s[2:].strip()]
            i += 1
            while i < len(lines) and lines[i].strip().startswith("//") \
                    and not lines[i].strip().startswith("///") \
                    and not lines[i].strip().startswith("// SAFETY:"):
                body.append(lines[i].strip()[2:].strip())
                i += 1
            out.append(Comment(file, start + 1, " ".join(body)))
            continue
        i += 1
    return out


def register_ids(text: str) -> set[str]:
    """The `I<n>` ids with a row in the invariant register."""
    return set(REGISTER_ROW.findall(text))


def comment_errors(index: ModuleIndex, mod: Module | None, c: Comment, ids: set[str]) -> list[str]:
    """What is wrong with one SAFETY comment."""
    errs: list[str] = []
    for m in INVARIANT_IDS.finditer(c.text):
        for rid in ID.findall(m.group(1)):
            if rid not in ids:
                errs.append(f"invariant {rid} has no row in {INVARIANTS}")
    text = c.text.replace("`", " ")
    if HERE.search(text):
        return errs
    paths = PATH.findall(text)
    if mod is None or not any(resolves(index, mod, p) for p in paths):
        named = ", ".join(paths) if paths else "no `::` path"
        errs.append(f"names neither `here` nor a resolving path ({named})")
    return errs


def rs_files(root: Path) -> list[Path]:
    out: list[Path] = []
    for d in SCOPE:
        base = root / d
        if base.is_dir():
            out.extend(p for p in base.rglob("*.rs") if "target" not in p.parts)
    return sorted(out)


def check_tree(root: Path = ROOT, pending: Iterable[str] = PENDING,
               stale: Iterable[tuple[str, str]] = STALE_CLAIMS) -> tuple[list[str], list[str]]:
    """(errors, failing files) for the tree at `root`."""
    index = ModuleIndex(root)
    inv = root / INVARIANTS
    ids = register_ids(inv.read_text(encoding="utf-8")) if inv.is_file() else set()
    pending = set(pending)
    errors: list[str] = []
    failing: list[str] = []
    for f in rs_files(root):
        rel = f.relative_to(root).as_posix()
        text = f.read_text(encoding="utf-8", errors="replace")
        mod = index.module_of(f)
        errs = []
        for c in safety_comments(text, f):
            for e in comment_errors(index, mod, c, ids):
                errs.append(f"{rel}:{c.line}: SAFETY comment {e}")
        if errs:
            failing.append(rel)
            if rel not in pending:
                errors.extend(errs)
        elif rel in pending:
            errors.append(f"{rel}: listed in PENDING but passes; remove the entry")
    for rel in sorted(pending):
        if not (root / rel).is_file():
            errors.append(f"{rel}: listed in PENDING but missing")
    for rel, pat in stale:
        f = root / rel
        if not f.is_file():
            errors.append(f"{rel}: STALE_CLAIMS row names a missing file")
            continue
        flat = re.sub(r"\s*\n\s*(?://[/!]?\s*)?", " ", f.read_text(encoding="utf-8"))
        if re.search(pat, flat):
            errors.append(f"{rel}: stale claim /{pat}/ is back (ROADMAP §10.1, F041)")
    return errors, failing


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="check_safety.py")
    ap.add_argument("--failing", action="store_true",
                    help="print the files that fail, PENDING ignored")
    args = ap.parse_args(argv if argv is not None else [])
    errors, failing = check_tree()
    if args.failing:
        for rel in failing:
            print(rel)
        return 0
    for e in errors:
        print(f"check_safety: {e}", file=sys.stderr)
    if errors:
        print("check_safety: a SAFETY comment names its invariant and where it is established "
              "(AGENTS.md rule 7, ROADMAP §10.1)", file=sys.stderr)
        return 1
    print(f"check_safety: ok ({len(failing)} files pending)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
