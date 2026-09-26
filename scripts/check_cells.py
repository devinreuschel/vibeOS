#!/usr/bin/env python3
"""Cells and their soundness rules (DESIGN §2.3, AGENTS.md rule 6).

- Every function in MUST_BE_UNSAFE exists and is declared `unsafe fn`.
- Generic `Sync` impls live in src/cell.rs; `static mut` only in catch.
"""

from __future__ import annotations

import re
import sys
from collections.abc import Mapping, Sequence
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
]

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
        if "unsafe impl<T> Sync" in ln and path != "src/cell.rs":
            errors.append(f"generic Sync outside cell.rs: {where}")
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
    errors += must_be_unsafe_errors(files)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("check_cells: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
