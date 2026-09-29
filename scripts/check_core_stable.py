#!/usr/bin/env python3
"""The portable crate's byte parsers deny panicking indexing and arithmetic
(ROADMAP §10.1), and the crate holds no assembly and no `cfg(target_arch)`
(ROADMAP §10.3, DESIGN §11.1).

`vibeos-core` builds on stable Rust (DESIGN §1.1): `make check` builds it with
its MSRV (`make check-msrv`), which rejects a feature attribute however it is
formatted and any API or syntax newer than the MSRV, so this script no longer
searches for one.

`missing_parser_attrs` checks each `PARSERS` row for its `indexing_slicing`
and `arithmetic_side_effects` attribute, except rows on `PARSERS_PENDING`,
which fail once the attribute exists.

`find_arch_code` finds `asm!`, `global_asm!`, `naked_asm!`, and any
`target_arch` token outside comments. `main` runs it on every file
`core_files` walks: each `.rs` under `crates/core/src`, test modules included,
and each file a `#[path]` attribute there names (`src/cell.rs`), so a host test
of a port's assembly lives in a crate outside `vibeos-core` (`tests/hostlib`).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CORE_ROOT = ROOT / "crates" / "core" / "src" / "lib.rs"

# The byte parsers (ROADMAP §10.1, C-LINTS): each denies `indexing_slicing` and
# `arithmetic_side_effects`, which `overflow-checks` and bounds checks would
# otherwise turn into panics on crafted input (AGENTS.md rule 4). A row is
# (path under crates/core/src, kind, target):
#   "inner": an inner `#![deny(..)]` naming both lints;
#   "fn":    an outer `#[deny(..)]` naming both, on `fn target` (or on the
#            method `Type::method` in an `impl Type` block);
#   "allow": an inner `#![allow(..)]` naming both whose reason names `target`.
# A slice that adds a byte parser appends its row (C-LINTS).
PARSER_LINTS = ("clippy::indexing_slicing", "clippy::arithmetic_side_effects")
PARSERS: tuple[tuple[str, str, str], ...] = (
    ("acpi/mod.rs", "inner", ""),
    ("block/part.rs", "inner", ""),
    ("fs/fat/mod.rs", "inner", ""),
    ("dev/pci.rs", "inner", ""),
    ("proc/elf.rs", "inner", ""),
    ("dev/virtio.rs", "fn", "read_modern_caps"),
    ("shell/mod.rs", "fn", "tokenize"),
    ("console/kbd.rs", "fn", "Decoder::feed"),
    ("fs/vibefs/mod.rs", "allow", "§14.8"),
)
# Rows whose attribute the sweep that owns the module adds (ROADMAP §10.1),
# as `path` or `path::target`. A pending row whose attribute exists fails.
PARSERS_PENDING: tuple[str, ...] = (
)

ATTR_START = re.compile(r"#!?\[")
FN_LINE = re.compile(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:const\s+|unsafe\s+)*fn\s+(\w+)\b")
IMPL_LINE = re.compile(r"^(\s*)impl\b(?:\s*<[^>]*>)?\s+(\w+)\b[^{]*\{")


def attr_blocks(text: str, opener: str) -> list[str]:
    """Each whole attribute that starts with `opener` (`#![deny(` and so on)."""
    out = []
    i = text.find(opener)
    while i >= 0:
        depth = 0
        j = i
        while j < len(text):
            if text[j] == "[":
                depth += 1
            elif text[j] == "]":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        out.append(text[i: j + 1])
        i = text.find(opener, j)
    return out


def names_both(attr: str) -> bool:
    return all(lint in attr for lint in PARSER_LINTS)


def outer_attrs(lines: list[str], i: int) -> str:
    """The attribute, doc and comment lines directly above line `i`, joined
    (a rustfmt-wrapped attribute's inner lines included)."""
    j = i - 1
    while j >= 0 and lines[j].strip().startswith(("#[", "//", ")]", "clippy::", "reason")):
        j -= 1
    return "\n".join(lines[j + 1: i])


def find_fn(lines: list[str], target: str) -> int | None:
    """Line index of `fn target` at the top level, or of `Type::method`."""
    if "::" in target:
        ty, name = target.split("::", 1)
        for i, line in enumerate(lines):
            m = IMPL_LINE.match(line)
            if m and m.group(2) == ty and " for " not in line:
                indent = m.group(1)
                for k in range(i + 1, len(lines)):
                    if lines[k].startswith(indent + "}"):
                        break
                    fm = FN_LINE.match(lines[k])
                    if fm and fm.group(2) == name and fm.group(1) == indent + "    ":
                        return k
        return None
    for i, line in enumerate(lines):
        fm = FN_LINE.match(line)
        if fm and fm.group(2) == target and fm.group(1) == "":
            return i
    return None


def has_parser_attr(text: str, kind: str, target: str) -> bool | None:
    """Whether the row's attribute is present; None when its fn is missing."""
    if kind == "inner":
        return any(names_both(a) for a in attr_blocks(text, "#![deny("))
    if kind == "allow":
        return any(names_both(a) and target in a for a in attr_blocks(text, "#![allow("))
    lines = text.splitlines()
    i = find_fn(lines, target)
    if i is None:
        return None
    return any(names_both(a) for a in attr_blocks(outer_attrs(lines, i), "#[deny("))


def missing_parser_attrs(core_src: Path,
                         parsers: tuple[tuple[str, str, str], ...] = PARSERS,
                         pending: tuple[str, ...] = PARSERS_PENDING) -> list[str]:
    """Errors for the listed parsers under `core_src` (crates/core/src)."""
    errors = []
    for rel, kind, target in parsers:
        key = f"{rel}::{target}" if kind == "fn" else rel
        f = core_src / rel
        if not f.is_file():
            errors.append(f"{rel}: listed parser file is missing")
            continue
        have = has_parser_attr(f.read_text(encoding="utf-8"), kind, target)
        if have is None:
            errors.append(f"{rel}: listed parser fn `{target}` is missing")
        elif have and key in pending:
            errors.append(f"{key}: in PARSERS_PENDING but its attribute exists; remove the entry")
        elif not have and key not in pending:
            what = {"inner": "an inner `#![deny(..)]`", "fn": f"`#[deny(..)]` on `{target}`",
                    "allow": f"an inner `#![allow(..)]` whose reason names {target}"}[kind]
            errors.append(f"{key}: needs {what} naming {' and '.join(PARSER_LINTS)} "
                          "(ROADMAP §10.1, AGENTS.md rule 4)")
    return errors


# `asm!` and its module forms, `global_asm!`, `naked_asm!`; not `my_asm!`.
ARCH_MACRO = re.compile(r"(?<![\w])(?:global_|naked_)?asm\s*!")
TARGET_ARCH = re.compile(r"\btarget_arch\b")
PATH_ATTR = re.compile(r'#\[\s*path\s*=\s*"([^"]+)"\s*\]')
RAW_STR = re.compile(r'b?r(#*)"')
# A char literal, so `'"'` opens no string: `'x'`, `'\''`, `'\u{..}'` and so on.
CHAR_LIT = re.compile(r"b?'(?:[^'\\\n]|\\(?:[^u\n]|u\{[0-9a-fA-F]{1,6}\}))'")


def strip_comments(text: str) -> str:
    """`text` with `//` and (nested) `/* */` comments blanked, newlines kept,
    so line numbers survive. String, raw string and char literals are kept
    whole, so a `//` inside one opens no comment."""
    out: list[str] = []
    i, n, depth = 0, len(text), 0
    while i < n:
        c = text[i]
        if depth:
            if text.startswith("/*", i):
                depth += 1
                i += 2
            elif text.startswith("*/", i):
                depth -= 1
                i += 2
            else:
                out.append("\n" if c == "\n" else " ")
                i += 1
            continue
        if text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
            continue
        if text.startswith("/*", i):
            depth = 1
            i += 2
            continue
        raw = RAW_STR.match(text, i)
        if raw and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            end = text.find('"' + raw.group(1), raw.end())
            j = n if end < 0 else end + 1 + len(raw.group(1))
            out.append(text[i:j])
            i = j
            continue
        char = CHAR_LIT.match(text, i)
        if char:
            out.append(char.group(0))
            i = char.end()
            continue
        if c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append(text[i: j + 1])
            i = j + 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def find_arch_code(text: str) -> list[int]:
    """1-based lines of `text` that hold `asm!`, `global_asm!`, `naked_asm!`, or
    a `target_arch` token outside comments."""
    lines = strip_comments(text).splitlines()
    return [k + 1 for k, line in enumerate(lines)
            if ARCH_MACRO.search(line) or TARGET_ARCH.search(line)]


def core_files(src: Path) -> list[Path]:
    """Every `.rs` under `src`, and each file a `#[path]` attribute in one of
    them names (followed on from those files too), sorted and resolved."""
    todo = sorted(p.resolve() for p in src.rglob("*.rs"))
    seen: set[Path] = set()
    while todo:
        f = todo.pop()
        if f in seen or not f.is_file():
            continue
        seen.add(f)
        for m in PATH_ATTR.finditer(strip_comments(f.read_text(encoding="utf-8"))):
            todo.append((f.parent / m.group(1)).resolve())
    return sorted(seen)


def arch_code_errors(src: Path) -> list[str]:
    """One error per line of a `core_files` file that `find_arch_code` flags."""
    errors = []
    for f in core_files(src):
        for line in find_arch_code(f.read_text(encoding="utf-8")):
            try:
                name = f.relative_to(ROOT)
            except ValueError:
                name = f
            errors.append(f"{name}:{line}: assembly or `target_arch` in vibeos-core; move it to "
                          "the port's hardware half, or its host test to tests/hostlib "
                          "(ROADMAP §10.3, DESIGN §11.1)")
    return errors


def main() -> int:
    errors = missing_parser_attrs(CORE_ROOT.parent)
    errors += arch_code_errors(CORE_ROOT.parent)
    for e in errors:
        print(f"check_core_stable: {e}", file=sys.stderr)
    if errors:
        return 1
    print("check_core_stable: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
