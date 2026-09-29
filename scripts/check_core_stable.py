#!/usr/bin/env python3
"""The portable crate builds on stable Rust (DESIGN §1.1), and its byte
parsers deny panicking indexing and arithmetic (ROADMAP §10.1).

`vibeos-core` enables no language or library feature: Verus (ROADMAP Phase 38),
Kani, loom, and Miri each pin their own toolchain, and they must keep building
the crate. Feature attributes are crate-level, so the crate root is the only
file to check. Nightly features stay in the kernel binary (`src/main.rs`).

`missing_parser_attrs` checks each `PARSERS` row for its `indexing_slicing`
and `arithmetic_side_effects` attribute, except rows on `PARSERS_PENDING`,
which fail once the attribute exists.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CORE_ROOT = ROOT / "crates" / "core" / "src" / "lib.rs"

# `#![feature(..)]` or `#![cfg_attr(<cond>, feature(..))]`; `feature = "std"`
# inside a cfg predicate is a Cargo feature, not a language feature.
FEATURE_ATTR = re.compile(r"^\s*#!\[\s*(?:cfg_attr\s*\(.*,\s*)?feature\s*\(")


def find_features(text: str) -> list[int]:
    """1-based line numbers of crate-level feature attributes."""
    return [n for n, line in enumerate(text.splitlines(), start=1) if FEATURE_ATTR.match(line)]


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
    ("boot/cmdline.rs", "inner", ""),
    ("boot/mod.rs", "fn", "parse_fw_cfg_dir_count"),
    ("boot/mod.rs", "fn", "parse_fw_cfg_dir_entry"),
    ("boot/mod.rs", "fn", "fw_cfg_dma_access"),
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


def main() -> int:
    lines = find_features(CORE_ROOT.read_text(encoding="utf-8"))
    rel = CORE_ROOT.relative_to(ROOT)
    for n in lines:
        msg = f"{rel}:{n}: vibeos-core enables a nightly feature (DESIGN §1.1)"
        print(msg, file=sys.stderr)
    errors = missing_parser_attrs(CORE_ROOT.parent)
    for e in errors:
        print(f"check_core_stable: {e}", file=sys.stderr)
    if lines or errors:
        return 1
    print("check_core_stable: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
