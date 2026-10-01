#!/usr/bin/env python3
"""One entry path: no `x86-interrupt` handler outside `src/arch/` (AGENTS.md rule 1).

Every IDT vector enters through a stub that `src/arch/x86_64/idt.rs` generates, and
`idt::set_handler` takes a plain body fn (ROADMAP §10.6, DESIGN §5.10 rule 1).
Three rules:
- `x86_interrupt_outside_arch`: an `extern "x86-interrupt"` in a `.rs` file
  under `src/`, `crates/`, `user/`, or `tests/`, outside `src/arch/`, fails.
  Text after `//` is ignored.
- `stale_abi_feature`: `src/main.rs` enabling `abi_x86_interrupt` fails when
  no `extern "x86-interrupt"` is left anywhere.
- `nomem_toggle`: an `asm!` whose template toggles IF or AC (`cli`, `sti`,
  `stac`, `clac`, or `msr daifset`/`msr daifclr`) and whose options include
  `nomem` fails, except the `sti; hlt` idle pair: enabling and disabling
  interrupts and opening and closing the SMAP window are compiler barriers
  (ROADMAP §10.3, F091).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402

SCOPE = ["src", "crates", "user", "tests"]
ARCH = "src/arch/"
MAIN = "src/main.rs"
ABI = re.compile(r'extern\s+"x86-interrupt"')
FEATURE = re.compile(r"#!\[feature\([^)]*\babi_x86_interrupt\b")


def _code(line: str) -> str:
    """`line` less a `//` comment."""
    return line.split("//", 1)[0]


def _abi_lines(files: dict[str, str]) -> list[tuple[str, int]]:
    out: list[tuple[str, int]] = []
    for path in sorted(files):
        if not path.endswith(".rs"):
            continue
        for n, line in enumerate(files[path].splitlines(), start=1):
            if ABI.search(_code(line)):
                out.append((path, n))
    return out


def x86_interrupt_outside_arch(files: dict[str, str]) -> list[str]:
    """Each `extern "x86-interrupt"` in `files` outside `src/arch/`."""
    return [f'{path}:{n}: extern "x86-interrupt" outside {ARCH} (AGENTS.md rule 1)'
            for path, n in _abi_lines(files) if not path.startswith(ARCH)]


def stale_abi_feature(main_text: str, files: dict[str, str]) -> list[str]:
    """The feature line in `main_text` when `files` has no handler left."""
    if _abi_lines(files):
        return []
    return [f"{MAIN}:{n}: abi_x86_interrupt enabled with no x86-interrupt fn left"
            for n, line in enumerate(main_text.splitlines(), start=1)
            if FEATURE.search(_code(line))]


ASM = re.compile(r"\basm!\s*\(")
TOGGLES = frozenset({"cli", "sti", "stac", "clac"})
DAIF = frozenset({"daifset", "daifclr"})
IDLE = ["sti", "hlt"]
CHAR = re.compile(r"b?'(?:\\u\{[0-9A-Fa-f]+\}|\\.|[^\\'\n])'")


def _skip_str(text: str, i: int) -> tuple[int, str]:
    """End of the string literal starting at `text[i]`, and its contents.

    Handles `"..."` with escapes and raw strings `r"..."`, `r#"..."#`.
    Returns `(len(text), ...)` for an unterminated literal.
    """
    j = i
    if text[j] == "r":
        j += 1
        hashes = 0
        while j < len(text) and text[j] == "#":
            hashes += 1
            j += 1
        close = '"' + "#" * hashes
        end = text.find(close, j + 1)
        if end < 0:
            return len(text), text[j + 1:]
        return end + len(close), text[j + 1:end]
    j += 1
    out: list[str] = []
    while j < len(text):
        c = text[j]
        if c == "\\":
            out.append(text[j:j + 2])
            j += 2
            continue
        if c == '"':
            return j + 1, "".join(out)
        out.append(c)
        j += 1
    return j, "".join(out)


def _is_raw_start(text: str, i: int) -> bool:
    """Whether `text[i]` starts a raw string literal (`r"` or `r#`)."""
    if text[i] != "r" or (i > 0 and (text[i - 1].isalnum() or text[i - 1] == "_")):
        return False
    j = i + 1
    while j < len(text) and text[j] == "#":
        j += 1
    return j < len(text) and text[j] == '"'


def _skip_comment(text: str, i: int) -> int:
    """End of the comment or char literal starting at `text[i]`, or `i` when
    there is none. A char literal such as `'"'` would otherwise open a
    string."""
    m = CHAR.match(text, i)
    if m is not None and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
        return m.end()
    if text.startswith("//", i):
        end = text.find("\n", i)
        return len(text) if end < 0 else end
    if text.startswith("/*", i):
        depth, j = 1, i + 2
        while j < len(text) and depth:
            if text.startswith("/*", j):
                depth, j = depth + 1, j + 2
            elif text.startswith("*/", j):
                depth, j = depth - 1, j + 2
            else:
                j += 1
        return j
    return i


def _asm_calls(text: str) -> list[tuple[int, str, str]]:
    """Each `asm!(...)` in `text`: its line, joined template, and the rest.

    Comments and string literals outside the call are skipped, so an
    `asm!` in a comment or a string is not one. `global_asm!` and
    `naked_asm!` are not matched: `\\b` does not split `_asm`.
    """
    out: list[tuple[int, str, str]] = []
    i = 0
    while i < len(text):
        c = text[i]
        end = _skip_comment(text, i)
        if end != i:
            i = end
            continue
        if c == '"' or _is_raw_start(text, i):
            i, _ = _skip_str(text, i)
            continue
        m = ASM.match(text, i)
        if m is None or (i > 0 and (text[i - 1].isalnum() or text[i - 1] == "_")):
            i += 1
            continue
        line = text.count("\n", 0, i) + 1
        j, depth = m.end(), 1
        template: list[str] = []
        rest: list[str] = []
        while j < len(text) and depth:
            end = _skip_comment(text, j)
            if end != j:
                j = end
                continue
            ch = text[j]
            if ch == '"' or _is_raw_start(text, j):
                j, lit = _skip_str(text, j)
                if depth == 1:
                    template.append(lit)
                continue
            if ch in "([{":
                depth += 1
            elif ch in ")]}":
                depth -= 1
                if not depth:
                    break
            rest.append(ch)
            j += 1
        out.append((line, "\n".join(template), "".join(rest)))
        i = j + 1
    return out


def _instructions(template: str) -> list[list[str]]:
    """`template` split into instructions on `;` and newlines (a literal
    newline or a `\\n` escape), each a list of its lowercased words."""
    out: list[list[str]] = []
    for part in re.split(r"[;\n]|\\n", template):
        words = re.findall(r"[A-Za-z_][A-Za-z0-9_.]*", part)
        if words:
            out.append([w.lower() for w in words])
    return out


def _toggles(insns: list[list[str]]) -> bool:
    for words in insns:
        if words[0] in TOGGLES:
            return True
        if words[0] == "msr" and len(words) > 1 and words[1] in DAIF:
            return True
    return False


def _has_nomem(rest: str) -> bool:
    return any(re.search(r"\bnomem\b", m)
               for m in re.findall(r"\boptions\s*\(([^)]*)\)", rest))


def nomem_toggle(files: dict[str, str]) -> list[str]:
    """Each `asm!` in `files` that toggles IF or AC and is declared `nomem`."""
    out: list[str] = []
    for path in sorted(files):
        if not path.endswith(".rs"):
            continue
        for line, template, rest in _asm_calls(files[path]):
            insns = _instructions(template)
            if [w[0] for w in insns] == IDLE or not _toggles(insns):
                continue
            if _has_nomem(rest):
                out.append(f"{path}:{line}: asm! toggles IF or AC and is declared nomem "
                           "(ROADMAP §10.3, F091)")
    return out


def scoped_files(repo: Path = ROOT) -> dict[str, str]:
    """Tracked and untracked, not ignored, `.rs` files in scope."""
    out = gatelib.git(repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard",
                      "--", *SCOPE)
    files: dict[str, str] = {}
    for p in sorted({p for p in out.split("\0") if p.endswith(".rs")}):
        try:
            files[p] = (repo / p).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
    return files


def main(argv: list[str] | None = None) -> int:
    if argv:
        print("usage: check_entry.py", file=sys.stderr)
        return 2
    files = scoped_files()
    errors = x86_interrupt_outside_arch(files)
    errors += stale_abi_feature(files.get(MAIN, ""), files)
    errors += nomem_toggle(files)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("check_entry: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
