#!/usr/bin/env python3
"""Cells and their soundness rules (DESIGN §2.3, AGENTS.md rule 6).

- Every function in MUST_BE_UNSAFE exists and is declared `unsafe fn`.
- Each `unsafe impl` of `Send` or `Sync`, its header read whole, bounds
  every type parameter by `Send`, and a `Sync` impl for a type in
  SHARES_REF also by `Sync`; a generic one appears only in
  GENERIC_IMPL_FILES. None is for a type in NO_UNSAFE_IMPL.
- `static mut` only in catch; no `&'static mut` return; no
  `static_mut_refs` allow.
"""

from __future__ import annotations

import re
import sys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# (file, fn) pairs whose every declaration must be `unsafe fn`. The fn is a
# bare name, or `Type::name` for the one declared in an `impl` of `Type`
# where a file has two fns of one name. An entry leaves this list only in a
# commit that says why, such as a type that now proves the argument.
MUST_BE_UNSAFE: list[tuple[str, str]] = [
    ("src/cell.rs", "IrqCell::force_unlock"),
    ("src/log_init.rs", "force_unlock"),
    ("src/log_init.rs", "with_logger_unlocked"),
    ("src/log_init.rs", "dump_tail"),
    ("src/per_cpu_init.rs", "with_cpu"),
]

# The only files that may hold a generic `unsafe impl` of `Send` or `Sync`
# (one with a type parameter), so no new cell type appears elsewhere.
# src/kalloc.rs holds `TryArc`'s one bounded pair (C-KALLOC).
GENERIC_IMPL_FILES: tuple[str, ...] = ("src/cell.rs", "src/sync_init.rs", "src/kalloc.rs")

# Types that share `&T` between holders, whose `Sync` needs `T: Send + Sync`
# (AGENTS.md rule 6); every other type's `Sync` and `Send` need `T: Send`.
SHARES_REF: tuple[str, ...] = ("BootCell", "RwLock")

# Types that must stay `Sync` from their fields alone: an `unsafe impl` of
# `Send` or `Sync` for one fails anywhere, so each field stays atomic or
# set once before it is published (DESIGN §7.5).
NO_UNSAFE_IMPL: tuple[str, ...] = ("PerCpuRemote",)

_UNSAFE_IMPL = re.compile(r"\bunsafe\s+impl\b")
_AUTO_TRAITS = ("Send", "Sync")


@dataclass(frozen=True)
class Impl:
    """One `unsafe impl` header, joined from `unsafe impl` to its `{`.

    `params` holds the type parameters, lifetimes and const parameters
    dropped, each with its inline bounds; `where` holds the `where`
    predicates as (bounded type, bounds).
    """

    line: int
    trait: str
    self_ty: str
    params: tuple[tuple[str, str], ...]
    where: tuple[tuple[str, str], ...]

_FN_PREFIX = r"(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:unsafe\s+)?(?:extern\s+\"[^\"]*\"\s+)?"
_IMPL_HEAD = re.compile(r"\bimpl\b")


def strip_comments(text: str) -> str:
    """Blank comments, string and char literals, keeping every newline."""
    out: list[str] = []
    i, n = 0, len(text)

    def blank(s: str) -> str:
        return "".join(c if c == "\n" else " " for c in s)

    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(blank(text[i:j]))
            i = j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append(blank(text[i:j]))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append('"' + blank(text[i + 1 : j]) + '"')
            i = j + 1
        elif c == "'":
            m = re.match(r"'(?:\\.|\\u\{[0-9a-fA-F]+\}|[^\\'\n])'", text[i:])
            if m:
                out.append("' '" + " " * (len(m.group(0)) - 3))
                i += len(m.group(0))
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


def _block_end(text: str, open_brace: int) -> int:
    depth = 0
    for j in range(open_brace, len(text)):
        if text[j] == "{":
            depth += 1
        elif text[j] == "}":
            depth -= 1
            if depth == 0:
                return j
    return len(text)


def _header_end(text: str, start: int) -> int:
    """Index of the first `{` after `start` outside `<...>`, or -1."""
    angle = 0
    for j in range(start, len(text)):
        c = text[j]
        if c == "<":
            angle += 1
        elif c == ">" and angle and text[j - 1] != "-":
            angle -= 1
        elif c == "{" and angle == 0:
            return j
        elif c == ";" and angle == 0:
            return -1
    return -1


def _last_segment(ty: str) -> str:
    """`crate::a::B<'_, T>` -> `B`."""
    ty = ty.strip()
    angle = 0
    for j, c in enumerate(ty):
        if c == "<":
            if angle == 0:
                ty = ty[:j]
                break
            angle += 1
    ty = ty.strip().lstrip("&").strip()
    return ty.rsplit("::", 1)[-1].strip()


def _impl_spans(code: str, type_name: str) -> list[tuple[int, int]]:
    """Body spans of every `impl` block whose self type is `type_name`."""
    spans: list[tuple[int, int]] = []
    for m in _IMPL_HEAD.finditer(code):
        brace = _header_end(code, m.end())
        if brace < 0:
            continue
        head = code[m.end() : brace]
        head = re.sub(r"\bwhere\b.*", "", head, flags=re.S)
        if head.lstrip().startswith("<"):
            head = head[_generics_end(head) :]
        self_ty = re.split(r"\bfor\b", head)[-1]
        if _last_segment(self_ty) == type_name:
            spans.append((brace, _block_end(code, brace)))
    return spans


def _generics_end(head: str) -> int:
    """Index just past the `<...>` that `head` starts with (after spaces)."""
    start = head.index("<")
    angle = 0
    for j in range(start, len(head)):
        if head[j] == "<":
            angle += 1
        elif head[j] == ">" and head[j - 1] != "-":
            angle -= 1
            if angle == 0:
                return j + 1
    return len(head)


def split_top(text: str, sep: str = ",") -> list[str]:
    """Split at `sep` outside `<...>`, `(...)` and `[...]`; drop empty parts."""
    parts: list[str] = []
    depth, cur = 0, list[str]()
    for j, c in enumerate(text):
        if c in "<([":
            depth += 1
        elif c in ")]" or (c == ">" and (j == 0 or text[j - 1] != "-")):
            depth = max(depth - 1, 0)
        if c == sep and depth == 0:
            parts.append("".join(cur).strip())
            cur = []
        else:
            cur.append(c)
    parts.append("".join(cur).strip())
    return [p for p in parts if p]


def _param(p: str) -> tuple[str, str] | None:
    """`T: Send + ?Sized = X` -> ("T", "Send + ?Sized"); None for a lifetime
    or a const parameter."""
    p = " ".join(p.split())
    if p.startswith("'") or p.startswith("const "):
        return None
    head, _, bounds = p.partition(":")
    name = head.split("=")[0].strip()
    bounds = split_top(bounds, "=")[0] if bounds.strip() else ""
    return name, bounds.strip()


def unsafe_impls(text: str) -> list[Impl]:
    """Every `unsafe impl` in `text`, comments ignored."""
    code = strip_comments(text)
    impls: list[Impl] = []
    for m in _UNSAFE_IMPL.finditer(code):
        brace = _header_end(code, m.end())
        if brace < 0:
            continue
        head = " ".join(code[m.end() : brace].split())
        params: list[tuple[str, str]] = []
        if head.startswith("<"):
            end = _generics_end(head)
            for p in split_top(head[1 : end - 1]):
                param = _param(p)
                if param is not None:
                    params.append(param)
            head = head[end:].strip()
        head, _, where_text = head.partition(" where ")
        if head.endswith(" where"):
            head = head[: -len(" where")]
        parts = re.split(r"\s+for\s+", head, maxsplit=1)
        trait = parts[0].strip().lstrip("!")
        self_ty = parts[1].strip() if len(parts) > 1 else ""
        where: list[tuple[str, str]] = []
        for pred in split_top(where_text):
            lhs, _, bounds = pred.partition(":")
            where.append((lhs.strip(), bounds.strip()))
        impls.append(
            Impl(
                line=line_of(code, m.start()),
                trait=_last_segment(trait),
                self_ty=_last_segment(self_ty),
                params=tuple(params),
                where=tuple(where),
            )
        )
    return impls


def _bound_traits(bounds: str) -> set[str]:
    """`?Sized + core::marker::Send + 'a` -> {"Send"}: `?` bounds and
    lifetimes are no bound."""
    out: set[str] = set()
    for b in split_top(bounds, "+"):
        if b.startswith("?") or b.startswith("'"):
            continue
        out.add(_last_segment(b))
    return out


def impl_errors(path: str, text: str) -> list[str]:
    """The `unsafe impl` rules for one file (DESIGN §2.3, AGENTS.md rule 6).

    One error per failing impl, at the line of its `unsafe impl`.
    """
    errors: list[str] = []
    for imp in unsafe_impls(text):
        if imp.trait not in _AUTO_TRAITS:
            continue
        problems: list[str] = []
        if imp.self_ty in NO_UNSAFE_IMPL:
            problems.append(f"{imp.self_ty} must be {imp.trait} from its fields alone")
        if imp.params and path not in GENERIC_IMPL_FILES:
            problems.append(
                "a generic impl belongs only in " + ", ".join(GENERIC_IMPL_FILES)
            )
        need = ["Send"]
        if imp.trait == "Sync" and imp.self_ty in SHARES_REF:
            need.append("Sync")
        for name, inline in imp.params:
            have = _bound_traits(inline)
            for lhs, bounds in imp.where:
                if lhs == name:
                    have |= _bound_traits(bounds)
            missing = [t for t in need if t not in have]
            if missing:
                problems.append(f"{name} is not bounded by {' + '.join(missing)}")
        if problems:
            errors.append(
                f"{path}:{imp.line}: unsafe impl {imp.trait} for {imp.self_ty}: "
                + "; ".join(problems)
            )
    return errors


def must_be_unsafe_errors(
    files: Mapping[str, str], entries: Sequence[tuple[str, str]] = MUST_BE_UNSAFE
) -> list[str]:
    """Each (file, fn) entry names at least one fn, and every one is `unsafe fn`."""
    errors: list[str] = []
    for path, entry in entries:
        text = files.get(path)
        if text is None:
            errors.append(f"{path}: missing file for must-be-unsafe {entry}")
            continue
        code = strip_comments(text)
        owner, _, name = entry.rpartition("::")
        decl = re.compile(rf"(?<![\w:]){_FN_PREFIX}fn\s+{re.escape(name)}\b")
        spans = _impl_spans(code, owner) if owner else [(0, len(code))]
        found = [m for m in decl.finditer(code) if any(a <= m.start() <= b for a, b in spans)]
        if not found:
            errors.append(f"{path}: must-be-unsafe fn {entry} not found")
        for m in found:
            if not re.search(r"\bunsafe\s", m.group(0)):
                errors.append(
                    f"{path}:{line_of(code, m.start())}: {entry} must be declared `unsafe fn`"
                )
    return errors


def legacy_errors(path: str, text: str) -> list[str]:
    """The pre-parser rules, one source line at a time."""
    errors: list[str] = []
    for n, ln in enumerate(text.splitlines(), 1):
        where = f"{path}:{n}:{ln}"
        if "static mut" in ln and path != "src/arch/catch.rs":
            errors.append(f"static mut outside catch.rs: {where}")
        if "-> &'static mut" in ln:
            errors.append(f"function returns &'static mut: {where}")
        if "static_mut_refs" in ln:
            errors.append(f"static_mut_refs allow/expect: {where}")
    return errors


def read_tree(root: Path = ROOT) -> dict[str, str]:
    return {
        p.relative_to(root).as_posix(): p.read_text(encoding="utf-8")
        for p in sorted((root / "src").rglob("*.rs"))
    }


def main() -> int:
    files = read_tree()
    errors: list[str] = []
    for path, text in files.items():
        errors += legacy_errors(path, text)
        errors += impl_errors(path, text)
    errors += must_be_unsafe_errors(files)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("check_cells: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
