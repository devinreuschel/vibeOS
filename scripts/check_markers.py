#!/usr/bin/env python3
"""The marker registry agrees with the kernel (ROADMAP §10.2, C-MARKERS).

`tests/contract/markers.toml` is the one list of the lines the harness knows
(`tests/harness/registry.py` loads it). This check fails when:

1. a `marker!` line in `src/` matches no row. A literal format string's
   `{...}` is a placeholder (`{{` and `}}` are braces), and an argument that
   is `marker::NAME` is replaced by that constant of
   `crates/core/src/marker.rs`. A fn or closure that passes one of its
   parameters to `marker!` (or to another such fn) is a forwarder: each
   string literal a call site passes it, anywhere in `src/` (a closure's in
   its own file), is a verbatim line. Any other argument is an error;
2. a `contract` or `diagnostic` row whose source is `kernel` matches no
   `marker!` line and no `PRINTERS` entry, in either direction;
3. a `crates/core/src/marker.rs` constant differs from its row: a `_PREFIX`
   constant must start a row's text, a `_SUFFIX` constant end one, and any
   other constant equal one;
4. a head signature of a `failure` row occurs in a `contract`, `diagnostic`
   or `test` row of the same source, or a pattern `failure` row matches one:
   such a row would fail a green boot. The box does not ask for this one;
and on a schema error, or a `section` that names no `### N.M` heading of
docs/ROADMAP.md. `PRINTERS` lists the kernel printers other than `marker!`
whose lines a driver reads; each entry's needle must still occur in its file.

Prints `check_markers: ok (<n> rows, <m> lines)`, or one `path:line: why`
per error on stderr and exits 1.
"""

from __future__ import annotations

import re
import sys
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from tests.harness import registry  # noqa: E402

SRC = "src"
MARKER_RS = "crates/core/src/marker.rs"
ROADMAP = "docs/ROADMAP.md"
REGISTRY = "tests/contract/markers.toml"

# What a placeholder renders as in a scanned line.
HOLE = "\x00"

# Kernel printers other than `marker!` whose lines a driver reads:
# (path, needle that must still occur in it, the line in format syntax, why).
PRINTERS: list[tuple[str, str, str, str]] = [
    ("crates/core/src/block/mod.rs", "pub fn write_marker(", "vibeOS: block: {} {} sectors",
     "portable, so it cannot call the kernel's `marker!`"),
    ("src/dev/pci_init.rs", "fn scan_line(", "vibeOS: pci: {} {:04x}:{:04x} {}",
     "one `write_line_with` for the prefix and `pci::write_lspci_line`"),
    ("src/log/diag.rs", "vibeOS: meminfo: total {} frames, free {}, used {}, largest order {}",
     "vibeOS: meminfo: total {} frames, free {}, used {}, largest order {}",
     "`diag::meminfo` writes to any `fmt::Write`: the shell's too"),
    ("src/log/diag.rs", "vibeOS: meminfo: leaked {} frames", "vibeOS: meminfo: leaked {} frames",
     "as above"),
    ("src/log/diag.rs", "vibeOS: meminfo: heap used {} B / capacity {} B",
     "vibeOS: meminfo: heap used {} B / capacity {} B", "as above"),
    ("src/log/diag.rs", "vibeOS: meminfo: kva used {} B", "vibeOS: meminfo: kva used {} B",
     "as above"),
    ("src/log/panic.rs", 'b"vibeOS: backtrace:"', "vibeOS: backtrace:",
     "the panic dump writes through `serial::raw::write_owner` (DESIGN §2.5 step 1)"),
    ("src/log/panic.rs", '"vibeOS: panic: cpu {c} stopped ({})"',
     "vibeOS: panic: cpu {} stopped ({})",
     "the dump's report on each other CPU, one `write_owner` line (DESIGN §2.5 step 1)"),
    ("src/log/panic.rs", '"vibeOS: panic: cpu {c} not stopped"',
     "vibeOS: panic: cpu {} not stopped",
     "as above"),
    ("src/log/panic.rs", '"vibeOS: panic: cpu {c} regs: rip=0x{:016x}',
     "vibeOS: panic: cpu {} regs: rip=0x{:016x} rsp=0x{:016x} rbp=0x{:016x} rflags=0x{:016x}",
     "as above"),
    ("src/log/panic_test.rs", 'b"vibeOS: panic_stop: owner nmi returned"',
     "vibeOS: panic_stop: owner nmi returned", "the panic-stop owner's `write_owner` line"),
    ("src/log/log_init.rs", '"vibeOS: dmesg: {}{} cpu{} {} {}"', "vibeOS: dmesg: {}{} cpu{} {} {}",
     "`dmesg` writes to any `fmt::Write` (AGENTS.md, Emit)"),
    ("src/log/log_init.rs", '"vibeOS: logrec: {}{} cpu{} {} {}"',
     "vibeOS: logrec: {}{} cpu{} {} {}", "the panic dump's log replay (AGENTS.md, Emit)"),
    ("src/arch/x86_64/idt.rs", 'dump(b"#BP"', "vibeOS: #BP rip={}",
     "the exception line is one raw write, as the dump's are"),
    ("src/proc/proc_init/mod.rs", '"user: syscall {name} nr={nr} = {ret}"',
     "user: syscall {} nr={} = {}", "a strace line is written whole on the syscall exit path"),
]

MARKER_CALL = re.compile(r"(?<![A-Za-z0-9_])(?:(?:\$?crate|vibeos)::)?marker!")
CONST_PATH = re.compile(r"^(?:(?:crate|vibeos)::)?marker::([A-Z0-9_]+)$")
IDENT = re.compile(r"^[a-z_][a-z0-9_]*$")
FN_DEF = re.compile(r"\bfn\s+([a-z_][a-z0-9_]*)\s*(?:<[^>]*>)?\s*\(")
CLOSURE_DEF = re.compile(r"\blet\s+([a-z_][a-z0-9_]*)\s*=\s*(?:move\s+)?\|([^|]*)\|")
CONST_DEF = re.compile(r'^\s*pub\s+const\s+([A-Z0-9_]+)\s*:\s*&str\s*=\s*"((?:[^"\\]|\\.)*)"\s*;',
                       re.M)
HEADING = re.compile(r"^### (\d+\.\d+) ", re.M)
ESCAPE = re.compile(r"\\(?:x([0-9a-fA-F]{2})|u\{([0-9a-fA-F]+)\}|\n\s*|(.))")
SIMPLE_ESCAPES = {"n": "\n", "r": "\r", "t": "\t", "0": "\0", "\\": "\\", '"': '"', "'": "'"}


@dataclass
class Call:
    """One line a `marker!` call, forwarder call site or printer prints:
    `rendered` is the line with each placeholder as `HOLE`."""

    path: str
    line: int
    rendered: str


def unescape(body: str) -> str:
    """A Rust string literal's body as the string it denotes."""

    def one(m: re.Match[str]) -> str:
        if m.group(1):
            return chr(int(m.group(1), 16))
        if m.group(2):
            return chr(int(m.group(2), 16))
        if m.group(3) is None:
            return ""  # a line continuation
        return SIMPLE_ESCAPES.get(m.group(3), m.group(3))

    return ESCAPE.sub(one, body)


def mask(text: str) -> str:
    """`text` with comments blanked and each string or char literal's body
    blanked, lengths and newlines kept, so structure can be scanned."""
    out = list(text)
    i, n = 0, len(text)

    def blank(a: int, b: int) -> None:
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
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
            blank(i, j)
            i = j
        elif (m := re.match(r'b?r(#*)"', text[i:])) and (i == 0 or not _word(text[i - 1])):
            end = text.find('"' + m.group(1), i + m.end())
            end = n if end < 0 else end
            blank(i + m.end(), end)
            i = end + 1 + len(m.group(1))
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            blank(i + 1, j)
            i = j + 1
        elif c == "'" and (m := re.match(r"'(?:\\.[^']*|[^\\'])'", text[i:])):
            blank(i + 1, i + m.end() - 1)
            i += m.end()
        else:
            i += 1
    return "".join(out)


def _word(c: str) -> bool:
    return c.isalnum() or c == "_"


def _close(masked: str, i: int) -> int:
    """The index of the bracket that closes the one at `i`."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[masked[i]]]
    for j in range(i + 1, len(masked)):
        c = masked[j]
        if c in pairs:
            stack.append(pairs[c])
        elif c in ")]}":
            if not stack or stack.pop() != c:
                return -1
            if not stack:
                return j
    return -1


def _split_args(masked: str, a: int, b: int) -> list[tuple[int, int]]:
    """The top-level comma-separated spans of `masked[a:b]`, stripped."""
    spans: list[tuple[int, int]] = []
    depth, start = 0, a
    for j in range(a, b):
        c = masked[j]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            spans.append((start, j))
            start = j + 1
    spans.append((start, b))
    out = []
    for s, e in spans:
        while s < e and masked[s].isspace():
            s += 1
        while e > s and masked[e - 1].isspace():
            e -= 1
        if s < e:
            out.append((s, e))
    return out


def _macro_bodies(masked: str) -> list[tuple[int, int]]:
    out = []
    for m in re.finditer(r"\bmacro_rules!\s*[A-Za-z_][A-Za-z0-9_]*\s*([({\[])", masked):
        end = _close(masked, m.end(1) - 1)
        if end > 0:
            out.append((m.start(), end))
    return out


def _literal(text: str, s: str) -> str | None:
    """The string a literal argument denotes, or None when it is not one."""
    m = re.fullmatch(r'"((?:[^"\\]|\\.)*)"', s, re.S)
    if m:
        return unescape(m.group(1))
    m = re.fullmatch(r'r(#*)"(.*)"\1', s, re.S)
    return m.group(2) if m else None


def render(fmt: str, args: Sequence[str] = (), consts: Mapping[str, str] | None = None) -> str:
    """A format string with each `{...}` as `HOLE`, except one whose argument
    is `marker::NAME`, which becomes the constant."""
    consts = consts or {}
    named: dict[str, str] = {}
    positional: list[str] = []
    for a in args:
        m = re.match(r"^([a-z_][a-z0-9_]*)\s*=(?!=)\s*(.*)$", a, re.S)
        if m:
            named[m.group(1)] = m.group(2).strip()
        else:
            positional.append(a.strip())

    def value(arg: str | None) -> str:
        if arg is None:
            return HOLE
        m = CONST_PATH.match(arg)
        if m and m.group(1) in consts:
            return consts[m.group(1)]
        return HOLE

    out: list[str] = []
    i, implicit = 0, 0
    while i < len(fmt):
        if fmt.startswith("{{", i) or fmt.startswith("}}", i):
            out.append(fmt[i])
            i += 2
        elif fmt[i] == "{":
            j = fmt.find("}", i)
            j = len(fmt) if j < 0 else j
            spec = fmt[i + 1 : j].split(":", 1)[0].strip()
            if spec == "":
                arg = positional[implicit] if implicit < len(positional) else None
                implicit += 1
            elif spec.isdigit():
                k = int(spec)
                arg = positional[k] if k < len(positional) else None
            else:
                arg = named.get(spec)
            out.append(value(arg))
            i = j + 1
        else:
            out.append(fmt[i])
            i += 1
    return "".join(out)


def parse_consts(text: str) -> dict[str, str]:
    """`marker.rs`'s `pub const NAME: &str = "..."` values, unescaped."""
    return {m.group(1): unescape(m.group(2)) for m in CONST_DEF.finditer(text)}


def _line(text: str, i: int) -> int:
    return text.count("\n", 0, i) + 1


@dataclass
class Forwarder:
    name: str
    index: int  # the parameter that reaches `marker!`
    path: str
    body: tuple[int, int]
    closure: bool
    param: str


def _param_names(params: str) -> list[str]:
    names = []
    for p in params.split(","):
        p = p.strip()
        if not p:
            continue
        name = p.split(":", 1)[0].strip().removeprefix("mut ").strip()
        names.append(name)
    return names


def _definitions(path: str, text: str, masked: str) -> list[tuple[str, list[str], int, int, bool]]:
    """(name, params, body start, body end, is closure) of each fn and
    closure in a file."""
    out = []
    for m in FN_DEF.finditer(masked):
        popen = m.end() - 1
        pclose = _close(masked, popen)
        if pclose < 0:
            continue
        brace = masked.find("{", pclose)
        semi = masked.find(";", pclose)
        if brace < 0 or (0 <= semi < brace):
            continue
        end = _close(masked, brace)
        if end > 0:
            out.append((m.group(1), _param_names(masked[popen + 1 : pclose]), brace, end, False))
    for m in CLOSURE_DEF.finditer(masked):
        depth, j = 0, m.end()
        while j < len(masked):
            c = masked[j]
            if c in "([{":
                depth += 1
            elif c in ")]}":
                depth -= 1
            elif c == ";" and depth == 0:
                break
            j += 1
        out.append((m.group(1), _param_names(m.group(2)), m.end(), j, True))
    return out


def _calls_of(masked: str, name: str, a: int = 0, b: int = -1) -> Iterable[tuple[int, int, int]]:
    """(name start, open paren, close paren) of each call of `name` in [a, b)."""
    b = len(masked) if b < 0 else b
    for m in re.finditer(r"(?<![A-Za-z0-9_.])" + re.escape(name) + r"\s*\(", masked[:b]):
        if m.start() < a:
            continue
        before = masked[max(0, m.start() - 3) : m.start()]
        if before.endswith("fn "):
            continue
        close = _close(masked, m.end() - 1)
        if close > 0:
            yield m.start(), m.end() - 1, close


def scan_calls(
    path: str, text: str, consts: Mapping[str, str] | None = None
) -> tuple[list[Call], list[str], list[tuple[str, int]]]:
    """The `marker!` lines of one file: (lines, errors, and the (param name,
    body start) of each non-literal argument, for the forwarder pass)."""
    masked = mask(text)
    skip = _macro_bodies(masked)
    calls: list[Call] = []
    errors: list[str] = []
    params: list[tuple[str, int]] = []
    for m in MARKER_CALL.finditer(masked):
        if any(a <= m.start() < b for a, b in skip):
            continue
        j = m.end()
        while j < len(masked) and masked[j].isspace():
            j += 1
        if j >= len(masked) or masked[j] not in "([{":
            continue
        close = _close(masked, j)
        if close < 0:
            errors.append(f"{path}:{_line(text, m.start())}: unbalanced marker! call")
            continue
        spans = _split_args(masked, j + 1, close)
        line = _line(text, m.start())
        if not spans:
            errors.append(f"{path}:{line}: marker! with no argument")
            continue
        args = [text[s:e] for s, e in spans]
        first = args[0].strip()
        lit = _literal(text, first)
        if lit is not None:
            calls.append(Call(path, line, render(lit, args[1:], consts)))
            continue
        cm = CONST_PATH.match(first)
        if cm is not None and consts is not None:
            if cm.group(1) not in consts:
                errors.append(f"{path}:{line}: marker::{cm.group(1)} is not in {MARKER_RS}")
            else:
                calls.append(Call(path, line, consts[cm.group(1)]))
            continue
        if IDENT.match(first) and len(args) == 1:
            params.append((first, m.start()))
            continue
        errors.append(
            f"{path}:{line}: marker! argument {first!r} is neither a literal nor marker::NAME"
        )
    return calls, errors, params


def _forwarders(files: Mapping[str, str]) -> tuple[list[Call], list[str]]:
    """Resolve every `marker!(<param>)`: the forwarding fns and closures, and
    the literal each of their call sites passes."""
    masks = {p: mask(t) for p, t in files.items()}
    defs = {p: _definitions(p, t, masks[p]) for p, t in files.items()}
    fwd: list[Forwarder] = []
    errors: list[str] = []
    for path, text in files.items():
        for param, at in scan_calls(path, text)[2]:
            owner = _innermost(defs[path], at, param)
            if owner is None:
                errors.append(f"{path}:{_line(text, at)}: marker!({param}) outside a fn that "
                              f"takes {param!r}")
                continue
            name, params, a, b, closure = owner
            fwd.append(Forwarder(name, params.index(param), path, (a, b), closure, param))
    # Transitive: a fn or closure that passes its own parameter on to one.
    changed = True
    while changed:
        changed = False
        for f in list(fwd):
            for path, text in files.items():
                if f.closure and path != f.path:
                    continue
                for start, popen, pclose in _calls_of(masks[path], f.name):
                    spans = _split_args(masks[path], popen + 1, pclose)
                    if f.index >= len(spans):
                        continue
                    arg = text[spans[f.index][0] : spans[f.index][1]].strip()
                    if not IDENT.match(arg):
                        continue
                    owner = _innermost(defs[path], start, arg)
                    if owner is None:
                        continue
                    name, params, a, b, closure = owner
                    new = Forwarder(name, params.index(arg), path, (a, b), closure, arg)
                    if not any(g.name == new.name and g.path == new.path for g in fwd):
                        fwd.append(new)
                        changed = True
    calls: list[Call] = []
    for f in fwd:
        for path, text in files.items():
            if f.closure and path != f.path:
                continue
            for start, popen, pclose in _calls_of(masks[path], f.name):
                spans = _split_args(masks[path], popen + 1, pclose)
                line = _line(text, start)
                if f.index >= len(spans):
                    continue
                arg = text[spans[f.index][0] : spans[f.index][1]].strip()
                lit = _literal(text, arg)
                if lit is not None:
                    calls.append(Call(path, line, lit))
                    continue
                owner = _innermost(defs[path], start, arg) if IDENT.match(arg) else None
                if owner is not None and any(
                    g.name == owner[0] and g.path == path for g in fwd
                ):
                    continue
                errors.append(f"{path}:{line}: {f.name}() is a marker! forwarder; its argument "
                              f"{arg!r} is not a string literal")
    return calls, errors


def _innermost(
    defs: list[tuple[str, list[str], int, int, bool]], at: int, param: str
) -> tuple[str, list[str], int, int, bool] | None:
    best = None
    for d in defs:
        if d[2] <= at < d[3] and param in d[1] and (best is None or d[2] > best[2]):
            best = d
    return best


def printer_calls(root: Path, printers: Sequence[tuple[str, str, str, str]]
                  ) -> tuple[list[Call], list[str]]:
    calls: list[Call] = []
    errors: list[str] = []
    for path, needle, pattern, _why in printers:
        try:
            text = (root / path).read_text(encoding="utf-8")
        except OSError:
            errors.append(f"{path}:0: PRINTERS entry {pattern!r}: file missing")
            continue
        at = text.find(needle)
        if at < 0:
            errors.append(f"{path}:0: PRINTERS entry {pattern!r}: needle {needle!r} not found")
            continue
        calls.append(Call(path, _line(text, at), render(pattern)))
    return calls, errors


def _row_line(raw: str, row: registry.Row) -> int:
    at = raw.find(f"text = {_toml_str(row.text)}")
    return _line(raw, at) if at >= 0 else 0


def _toml_str(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _hole_regex(rendered: str) -> str:
    return ".+".join(re.escape(p) for p in rendered.split(HOLE))


def _covers(row: registry.Row, call: Call) -> bool:
    return re.fullmatch(registry.row_regex(row.text), call.rendered, re.S) is not None


def _covered_by(row: registry.Row, call: Call) -> bool:
    rendered = registry.PLACEHOLDER.sub(HOLE, row.text)
    return re.fullmatch(_hole_regex(call.rendered), rendered, re.S) is not None


def check(
    root: Path = ROOT,
    printers: Sequence[tuple[str, str, str, str]] | None = None,
) -> tuple[list[str], int, int]:
    """(errors, rows, lines) for the tree at `root`."""
    printers = PRINTERS if printers is None else printers
    errors: list[str] = []
    try:
        raw = (root / REGISTRY).read_text(encoding="utf-8")
        rows = registry.parse_rows(raw, Path(REGISTRY))
    except (OSError, registry.RegistryError) as e:
        return [f"{REGISTRY}:0: {e}"], 0, 0

    roadmap = (root / ROADMAP).read_text(encoding="utf-8") if (root / ROADMAP).exists() else ""
    headings = {f"§{h}" for h in HEADING.findall(roadmap)}
    for row in rows:
        if row.section not in headings:
            errors.append(f"{REGISTRY}:{_row_line(raw, row)}: section {row.section} is no "
                          f"`### N.M` heading of {ROADMAP}")

    consts_text = (root / MARKER_RS).read_text(encoding="utf-8") if (root / MARKER_RS).exists() \
        else ""
    consts = parse_consts(consts_text)
    files = {
        str(p.relative_to(root)): p.read_text(encoding="utf-8")
        for p in sorted((root / SRC).rglob("*.rs"))
    }
    calls: list[Call] = []
    for path, text in files.items():
        found, errs, _ = scan_calls(path, text, consts)
        calls += found
        errors += errs
    found, errs = _forwarders(files)
    calls += found
    errors += errs
    pcalls, errs = printer_calls(root, printers)
    errors += errs

    # 1: every line matches a row.
    for call in calls + pcalls:
        if not any(_covers(row, call) for row in rows):
            shown = call.rendered.replace(HOLE, "{}")
            errors.append(f"{call.path}:{call.line}: {shown!r} matches no row of {REGISTRY}")

    # 2: every kernel contract or diagnostic row has a printer.
    for row in rows:
        if row.source != "kernel" or row.kind not in ("contract", "diagnostic"):
            continue
        if row.arch == "aarch64":
            continue
        if not any(_covers(row, c) or _covered_by(row, c) for c in calls + pcalls):
            errors.append(f"{REGISTRY}:{_row_line(raw, row)}: {row.kind} row {row.text!r} "
                          "matches no marker! line and no PRINTERS entry")

    # 3: every marker.rs constant is a row's text or fragment.
    for name, value in consts.items():
        if name.endswith("_PREFIX"):
            ok = any(r.text.startswith(value) for r in rows)
            what = "starts no row's text"
        elif name.endswith("_SUFFIX"):
            ok = any(r.text.endswith(value) for r in rows)
            what = "ends no row's text"
        else:
            ok = any(r.text == value for r in rows)
            what = "is no row's text"
        if not ok:
            at = consts_text.find(f"const {name}")
            errors.append(f"{MARKER_RS}:{_line(consts_text, at)}: {name} = {value!r} {what}")

    # 4: no row a green boot prints fails the run.
    for src in registry.SOURCES:
        heads = registry.signatures(rows, {src})
        pats = registry.failure_patterns(rows, {src})
        for row in rows:
            if row.kind == "failure" or row.source != src:
                continue
            text = registry.sample(row)
            bad = [h for h in heads if h in text] + [p.pattern for p in pats if p.search(text)]
            if bad:
                errors.append(f"{REGISTRY}:{_row_line(raw, row)}: {row.kind} row {row.text!r} "
                              f"would fail a boot on failure {bad[0]!r}")
    return errors, len(rows), len(calls) + len(pcalls)


def main() -> int:
    errors, nrows, nlines = check()
    if errors:
        for e in errors:
            print(e, file=sys.stderr)
        return 1
    print(f"check_markers: ok ({nrows} rows, {nlines} lines)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
