#!/usr/bin/env python3
"""The architecture seam and the x86 audit (ROADMAP §10.3, DESIGN §11.1).

`docs/ARCH.md` maps each row of PORTABILITY.md §11.1's seam table to the
modules that implement it, and lists every file outside `arch/` that keeps
x86_64 code behind `#[cfg(target_arch = "x86_64")]`. Rules:

- `rows`: ARCH.md's seam rows match §11.1's table one to one, in order.
- `seam`: each §11.1 trait row names a trait that
  `crates/core/src/arch/mod.rs` declares and lists among `Port`'s
  supertraits; `src/arch/current.rs` and `crates/core/src/arch/stub.rs` each
  hold the `assert_port::<Arch>()` assertion.
- `pure`: the x86_64 pure half's encodings exist, and `pub mod x86_64;` in
  `crates/core/src/arch/mod.rs` has no `cfg`.
- `dyn`: no `dyn Port` or `dyn` of a seam trait in `src/` or `crates/`.
- `paths`: every backticked path in ARCH.md exists, and every file under
  either `arch/x86_64/` appears in it.
- `audit`: the grep of ROADMAP §10.3's audit box (`asm!`, `x86`, and CR and
  MSR names) over `src/` and `crates/core/src/`, outside both `arch/`
  directories, with comments and string contents stripped: every hit is in
  `src/` and fenced, and its file is listed under ARCH.md's Fenced sites; a
  listed file with no hit left is stale.

A fence is `#[cfg(target_arch = "x86_64")]` on an item, statement, array
element, `use` or `mod` declaration: after the attribute (and any further
attributes) the unit runs to the first `;` or `,` at its depth, or, when a
`{` opens at that depth first, to the matching `}` and a trailing `;` or
`,`. A fenced `mod x;` fences `x.rs` or `x/mod.rs` and every file below it,
and `#![cfg(target_arch = "x86_64")]` fences its whole file the same way.
`not`, `any` and `all` forms are not fences.

`--list` prints each audit hit with its fence status and exits 0.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

ARCH_MD = "docs/ARCH.md"
PORTABILITY = "docs/PORTABILITY.md"
CORE_ARCH = "crates/core/src/arch/mod.rs"
CURRENT = "src/arch/current.rs"
STUB = "crates/core/src/arch/stub.rs"
PURE = tuple(
    f"crates/core/src/arch/x86_64/{n}.rs" for n in ("desc", "vectors", "pic", "apic", "paging")
)
PORT_DIRS = ("src/arch/x86_64", "crates/core/src/arch/x86_64")
AUDIT_ROOTS = ("src", "crates/core/src")
ARCH_DIRS = ("src/arch/", "crates/core/src/arch/")
DYN_ROOTS = ("src", "crates")
ASSERT_PORT = re.compile(r"assert_port\s*::\s*<\s*Arch\s*>\s*\(\s*\)")

AUDIT = re.compile(
    r"(?<![A-Za-z0-9_])(?:global_|naked_)?asm!"
    r"|(?<![A-Za-z0-9_])(?:x86|x86_64|[cC][rR][0-48]|rdmsr|wrmsr|MSR_[A-Za-z0-9_]*"
    r"|IA32_[A-Za-z0-9_]*|EFER|STAR|LSTAR|FMASK|FS_BASE|GS_BASE|KERNEL_GS_BASE)"
    r"(?![A-Za-z0-9_])"
)
FENCE = re.compile(r'#\s*\[\s*cfg\s*\(\s*target_arch\s*=\s*"x86_64"\s*\)\s*\]')
INNER_FENCE = re.compile(r'#!\s*\[\s*cfg\s*\(\s*target_arch\s*=\s*"x86_64"\s*\)\s*\]')
MOD_DECL = re.compile(r"\A\s*(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;")
PATH_ATTR = re.compile(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]')
LINK = re.compile(r"\[([^\]]*)\]\([^)]*\)")
TRAIT_CELL = re.compile(r"trait \(`([A-Za-z]+)`\)")
TICKED = re.compile(r"`([^`]+)`")


# ------------------------------------------------------------------ lexing


def _raw_string_end(text: str, i: int) -> tuple[int, int] | None:
    """For a raw string whose `r` is at `text[i]`: (body start, end past the
    closing quote and hashes), or None when `text[i]` starts no raw string."""
    j = i + 1
    while j < len(text) and text[j] == "#":
        j += 1
    if j >= len(text) or text[j] != '"':
        return None
    hashes = j - i - 1
    close = '"' + "#" * hashes
    end = text.find(close, j + 1)
    stop = len(text) if end < 0 else end + len(close)
    return j + 1, stop


CHAR_LIT = re.compile(r"'(?:\\u\{[0-9A-Fa-f]+\}|\\x[0-9A-Fa-f]{2}|\\.|[^\\'\n])'")


def _blank(s: str) -> str:
    """`s` with every character but a newline turned into a space."""
    return "".join("\n" if c == "\n" else " " for c in s)


def strip_code(text: str, strings: bool = True) -> str:
    """`text` with comments blanked and, when `strings`, the contents of string,
    raw-string, byte-string and char literals blanked too. Lengths and line
    breaks are kept, so offsets and line numbers still match `text`.
    Lifetimes (`'a`) are not char literals."""
    out: list[str] = []
    i = 0
    n = len(text)
    prev = ""
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            out.append(_blank(text[i:end]))
            i = end
            continue
        if text.startswith("/*", i):
            depth = 0
            j = i
            while j < n:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                    if depth == 0:
                        break
                else:
                    j += 1
            out.append(_blank(text[i:j]))
            i = j
            continue
        word_before = prev.isalnum() or prev == "_"
        if c in "rb" and not word_before:
            # r"..", r#".."#, br"..", b"..", b'.'
            k = i + 1 if c == "r" else (i + 2 if text.startswith("br", i) else -1)
            if c == "r" or k > 0:
                start = i if c == "r" else i + 1
                raw = _raw_string_end(text, start)
                if raw is not None:
                    body, stop = raw
                    out.append(text[i:body])
                    close_len = stop - body
                    inner_end = text.rfind('"', body, stop)
                    inner_end = stop if inner_end < body else inner_end
                    inner = text[body:inner_end]
                    out.append(_blank(inner) if strings else inner)
                    out.append(text[inner_end:stop])
                    del close_len
                    prev = '"'
                    i = stop
                    continue
            if c == "b" and i + 1 < n and text[i + 1] in "\"'":
                out.append("b")
                prev = "b"
                i += 1
                c = text[i]
                if c == "'":
                    m = CHAR_LIT.match(text, i)
                    if m:
                        lit = m.group(0)
                        out.append("'" + (_blank(lit[1:-1]) if strings else lit[1:-1]) + "'")
                        prev = "'"
                        i = m.end()
                        continue
                # a byte string falls through to the string case below
        if c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            j = min(j, n)
            inner = text[i + 1 : j]
            out.append('"' + (_blank(inner) if strings else inner))
            if j < n:
                out.append('"')
                j += 1
            prev = '"'
            i = j
            continue
        if c == "'":
            m = CHAR_LIT.match(text, i)
            if m:
                lit = m.group(0)
                out.append("'" + (_blank(lit[1:-1]) if strings else lit[1:-1]) + "'")
                prev = "'"
                i = m.end()
                continue
        out.append(c)
        prev = c
        i += 1
    return "".join(out)


def _line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


def _skip_ws(code: str, i: int) -> int:
    while i < len(code) and code[i].isspace():
        i += 1
    return i


def _match_close(code: str, i: int) -> int:
    """Index just past the bracket that closes the one at `code[i]`."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[code[i]]]
    j = i + 1
    while j < len(code) and stack:
        c = code[j]
        if c in pairs:
            stack.append(pairs[c])
        elif c in ")]}":
            if c != stack[-1]:
                return j + 1
            stack.pop()
        j += 1
    return j


def _unit_end(code: str, i: int) -> int:
    """End of the unit a fence attribute ending at `i` covers."""
    # Skip further outer attributes.
    while True:
        i = _skip_ws(code, i)
        if code.startswith("#", i) and not code.startswith("#!", i):
            k = _skip_ws(code, i + 1)
            if k < len(code) and code[k] == "[":
                i = _match_close(code, k)
                continue
        break
    depth = 0
    j = i
    while j < len(code):
        c = code[j]
        if c in "([":
            depth += 1
        elif c in ")]":
            if depth == 0:
                return j
            depth -= 1
        elif c == "{":
            if depth == 0:
                end = _match_close(code, j)
                k = _skip_ws(code, end)
                if k < len(code) and code[k] in ";,":
                    return k + 1
                return end
            depth += 1
        elif c == "}":
            if depth == 0:
                return j
            depth -= 1
        elif c in ";," and depth == 0:
            return j + 1
        j += 1
    return j


def fence_spans(text: str) -> list[tuple[int, int]]:
    """(start, end) offsets of each unit an outer fence covers, the
    attribute included. The attribute is found with string contents kept;
    the unit's brackets are matched with them blanked."""
    code = strip_code(text, strings=False)
    blank = strip_code(text)
    return [(m.start(), _unit_end(blank, m.end())) for m in FENCE.finditer(code)]


def fenced_lines(text: str) -> set[int]:
    """Line numbers (from 1) inside a fenced unit; every line of a file that
    carries `#![cfg(target_arch = "x86_64")]`."""
    code = strip_code(text, strings=False)
    if INNER_FENCE.search(code):
        return set(range(1, text.count("\n") + 2))
    lines: set[int] = set()
    for start, end in fence_spans(text):
        lines.update(range(_line_of(text, start), _line_of(text, max(start, end - 1)) + 1))
    return lines


def fenced_mods(text: str) -> list[tuple[str, str | None]]:
    """(name, `#[path]` value) of each `mod name;` a fence covers."""
    code = strip_code(text, strings=False)
    blank = strip_code(text)
    out = []
    for m in FENCE.finditer(code):
        end = _unit_end(blank, m.end())
        unit = code[m.end() : end]
        path = PATH_ATTR.search(unit)
        body = PATH_ATTR.sub("", unit)
        body = re.sub(r"#\s*\[[^\]]*\]", "", body)
        mm = MOD_DECL.match(body)
        if mm:
            out.append((mm.group(1), path.group(1) if path else None))
    return out


# ------------------------------------------------------------------ files


def _rel(root: Path, p: Path) -> str:
    return p.relative_to(root).as_posix()


def _rs_files(root: Path, top: str) -> list[str]:
    base = root / top
    if not base.is_dir():
        return []
    return sorted(_rel(root, p) for p in base.rglob("*.rs"))


def _read(root: Path, rel: str) -> str:
    try:
        return (root / rel).read_text(encoding="utf-8")
    except OSError:
        return ""


def _child_dir(rel: str) -> str:
    """The directory where `rel`'s `mod x;` children live."""
    p = Path(rel)
    if p.name in ("mod.rs", "lib.rs", "main.rs"):
        return p.parent.as_posix()
    return (p.parent / p.stem).as_posix()


def fenced_files(root: Path, files: list[str]) -> set[str]:
    """Files a fenced `mod` line or an inner fence covers, with every file
    below them."""
    prefixes: set[str] = set()
    exact: set[str] = set()
    for rel in files:
        text = _read(root, rel)
        if INNER_FENCE.search(strip_code(text, strings=False)):
            exact.add(rel)
            prefixes.add(_child_dir(rel) + "/")
        for name, path in fenced_mods(text):
            if path is not None:
                target = (Path(rel).parent / path).as_posix()
                exact.add(target)
                prefixes.add(_child_dir(target) + "/")
                continue
            d = _child_dir(rel)
            exact.add(f"{d}/{name}.rs")
            exact.add(f"{d}/{name}/mod.rs")
            prefixes.add(f"{d}/{name}/")
    return {f for f in files if f in exact or any(f.startswith(p) for p in prefixes)}


@dataclass(frozen=True)
class Hit:
    path: str
    line: int
    token: str
    fenced: bool


def audit(root: Path = ROOT) -> list[Hit]:
    """Every audit grep hit outside both `arch/` directories."""
    files: list[str] = []
    for top in AUDIT_ROOTS:
        files += _rs_files(root, top)
    files = sorted(set(files))
    whole = fenced_files(root, files)
    hits: list[Hit] = []
    for rel in files:
        if rel.startswith(ARCH_DIRS):
            continue
        text = _read(root, rel)
        code = strip_code(text)
        found = list(AUDIT.finditer(code))
        if not found:
            continue
        lines = fenced_lines(text)
        for m in found:
            ln = _line_of(code, m.start())
            hits.append(Hit(rel, ln, m.group(0), rel in whole or ln in lines))
    return hits


# ------------------------------------------------------------------ docs


def _cells(line: str) -> list[str]:
    s = line.strip()
    if s.startswith("|"):
        s = s[1:]
    if s.endswith("|"):
        s = s[:-1]
    return [c.strip() for c in s.split("|")]


def _table(lines: list[str], start: int) -> list[list[str]]:
    """Body rows of the first markdown table at or after `start`."""
    i = start
    while i < len(lines) and not lines[i].lstrip().startswith("|"):
        if lines[i].startswith("#") and i > start:
            return []
        i += 1
    rows = []
    i += 2  # header and rule
    while i < len(lines) and lines[i].lstrip().startswith("|"):
        rows.append(_cells(lines[i]))
        i += 1
    return rows


def _section(lines: list[str], heading: re.Pattern[str]) -> int:
    for i, line in enumerate(lines):
        if heading.match(line):
            return i
    return -1


def concern(cell: str) -> str:
    """A §11.1 Concern cell up to its first `:`, links reduced to their text."""
    text = LINK.sub(lambda m: m.group(1), cell)
    return text.split(":", 1)[0].strip()


def seam_table(text: str) -> list[list[str]]:
    lines = text.splitlines()
    i = _section(lines, re.compile(r"^## 11\.1\b"))
    return [] if i < 0 else _table(lines, i)


def arch_tables(text: str) -> dict[str, list[list[str]]]:
    lines = text.splitlines()
    out = {}
    for name in ("Seam rows", "Fenced sites"):
        i = _section(lines, re.compile(rf"^## {re.escape(name)}\s*$"))
        out[name] = [] if i < 0 else _table(lines, i)
    return out


def _paths_in(cell: str) -> list[str]:
    """Backticked paths in `cell`: text with a `/` and no space or glob `*`."""
    return [t for t in TICKED.findall(cell) if "/" in t and " " not in t and "*" not in t]


# ------------------------------------------------------------------ rules


def _rule_rows(root: Path, seam: list[list[str]], mine: list[list[str]]) -> list[str]:
    want = [concern(r[0]) for r in seam if r]
    have = [r[0] for r in mine if r]
    errs = []
    for k in range(max(len(want), len(have))):
        w = want[k] if k < len(want) else None
        h = have[k] if k < len(have) else None
        if w != h:
            errs.append(
                f"{ARCH_MD}:0: rows: seam row {k + 1} is {h!r}, PORTABILITY.md §11.1 has {w!r}"
            )
    return errs


def _supertraits(core: str) -> set[str]:
    m = re.search(r"pub\s+trait\s+Port\s*:(.*?)\{", core, re.S)
    if not m:
        return set()
    return set(re.findall(r"[A-Za-z_][A-Za-z0-9_]*", m.group(1)))


def seam_traits(seam: list[list[str]]) -> list[str]:
    names: list[str] = []
    for r in seam:
        if len(r) > 1:
            for t in TRAIT_CELL.findall(r[1]):
                if t not in names:
                    names.append(t)
    return names


def _rule_seam(root: Path, seam: list[list[str]]) -> list[str]:
    errs = []
    core = strip_code(_read(root, CORE_ARCH))
    supers = _supertraits(core)
    for t in seam_traits(seam):
        declared = re.search(rf"\bpub\s+trait\s+{t}\b", core) or re.search(
            rf"\bpub\s+use\s+[A-Za-z_:]*::{t}\s*;", core
        )
        if not declared:
            errs.append(f"{CORE_ARCH}:0: seam: trait {t} (PORTABILITY.md §11.1) is not declared")
        if t not in supers:
            errs.append(f"{CORE_ARCH}:0: seam: trait {t} is not a supertrait of Port")
    for rel in (CURRENT, STUB):
        if not ASSERT_PORT.search(strip_code(_read(root, rel))):
            errs.append(f"{rel}:0: seam: no `assert_port::<Arch>()` assertion")
    return errs


def _rule_pure(root: Path) -> list[str]:
    errs = [f"{p}:0: pure: missing" for p in PURE if not (root / p).is_file()]
    text = strip_code(_read(root, CORE_ARCH), strings=False)
    m = re.search(r"pub\s+mod\s+x86_64\s*;", text)
    if not m:
        errs.append(f"{CORE_ARCH}:0: pure: no `pub mod x86_64;`")
    else:
        before = text[: m.start()].rstrip()
        # The attributes directly on the declaration.
        attrs = re.findall(r"#\s*\[[^\]]*\]\s*$", before)
        while attrs:
            if "cfg" in attrs[0]:
                errs.append(f"{CORE_ARCH}:{_line_of(text, m.start())}: pure: `pub mod x86_64;` "
                            "carries a cfg")
                break
            before = before[: len(before) - len(attrs[0])].rstrip()
            attrs = re.findall(r"#\s*\[[^\]]*\]\s*$", before)
    return errs


def _rule_dyn(root: Path, traits: list[str]) -> list[str]:
    names = sorted(set(traits) | {"Port"})
    pat = re.compile(r"\bdyn\s+(?:[A-Za-z_][A-Za-z0-9_]*::)*(" + "|".join(names) + r")\b")
    errs = []
    for top in DYN_ROOTS:
        for rel in _rs_files(root, top):
            code = strip_code(_read(root, rel))
            for m in pat.finditer(code):
                errs.append(f"{rel}:{_line_of(code, m.start())}: dyn: `dyn {m.group(1)}`")
    return errs


def _rule_paths(root: Path, arch_text: str) -> list[str]:
    errs = []
    named: set[str] = set()
    for n, line in enumerate(arch_text.splitlines(), start=1):
        for path in _paths_in(line):
            named.add(path.rstrip("/"))
            if not (root / path).exists():
                errs.append(f"{ARCH_MD}:{n}: paths: `{path}` does not exist")
    for d in PORT_DIRS:
        base = root / d
        if not base.is_dir():
            continue
        for p in sorted(base.rglob("*")):
            if p.is_file():
                rel = _rel(root, p)
                if rel not in named:
                    errs.append(f"{rel}:0: paths: not in {ARCH_MD}")
    return errs


def _rule_audit(root: Path, fenced: list[list[str]]) -> list[str]:
    listed: dict[str, int] = {}
    for r in fenced:
        for p in _paths_in(r[0] if r else ""):
            listed[p] = listed.get(p, 0) + 1
    errs = []
    hit_files: set[str] = set()
    for h in audit(root):
        hit_files.add(h.path)
        if not h.path.startswith("src/"):
            errs.append(f"{h.path}:{h.line}: audit: `{h.token}` in vibeos-core outside arch/; "
                        "move it under crates/core/src/arch/")
        elif not h.fenced:
            errs.append(f"{h.path}:{h.line}: audit: `{h.token}` is neither moved nor fenced")
    for p in sorted(hit_files):
        if p.startswith("src/") and p not in listed:
            errs.append(f"{p}:0: audit: fenced file not listed under {ARCH_MD}'s Fenced sites")
    for p in sorted(listed):
        if p not in hit_files:
            errs.append(f"{ARCH_MD}:0: audit: `{p}` is listed under Fenced sites but has no hit")
        if listed[p] > 1:
            errs.append(f"{ARCH_MD}:0: audit: `{p}` is listed more than once")
    return errs


def check(root: Path = ROOT) -> list[str]:
    """Every rule's failures, as `path:line: rule: message`."""
    port = _read(root, PORTABILITY)
    arch_text = _read(root, ARCH_MD)
    if not arch_text:
        return [f"{ARCH_MD}:0: rows: missing"]
    seam = seam_table(port)
    if not seam:
        return [f"{PORTABILITY}:0: rows: no §11.1 table"]
    tables = arch_tables(arch_text)
    errs = _rule_rows(root, seam, tables["Seam rows"])
    errs += _rule_seam(root, seam)
    errs += _rule_pure(root)
    errs += _rule_dyn(root, seam_traits(seam))
    errs += _rule_paths(root, arch_text)
    errs += _rule_audit(root, tables["Fenced sites"])
    return errs


def main(argv: list[str] | None = None, root: Path = ROOT) -> int:
    args = sys.argv[1:] if argv is None else argv
    if args == ["--list"]:
        for h in audit(root):
            print(f"{h.path}:{h.line}: {h.token}: {'fenced' if h.fenced else 'UNFENCED'}")
        return 0
    if args:
        print("usage: check_arch.py [--list]", file=sys.stderr)
        return 2
    errs = check(root)
    for e in errs:
        print(e)
    if errs:
        print(f"check_arch: {len(errs)} failure(s)", file=sys.stderr)
        return 1
    print("check_arch: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
