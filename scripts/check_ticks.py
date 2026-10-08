#!/usr/bin/env python3
"""Every ticked ROADMAP box names its proof (ROADMAP §10.9, the check_ticks.py box).

For each non-merge commit of a pull request (`base..head`), the lines of
docs/ROADMAP.md that the commit changes to `- [x]` are its ticks: an added
`- [x]` line, unless that text is already `- [x]` at the merge base or the
pull request removes a `- [x]` line with the same text (this commit, or a
later one). A `Proves:` line that pairs only with a removed line is not an
error. So a ticked box whose text changes names its proof again; one reopened
or deleted needs none.

Each tick pairs with one `Proves: <proof>[ [<bracket>]][ (existing: <reason>)]
-- <prefix>` line of the commit's message, read anywhere in the message: the
prefix begins that ticked line and no other line the commit ticks (whitespace
collapsed). The line splits at its first ` -- `.

The proof exists at the head: a `make <target>` rule, a path (optionally
`<path>::<name>`), a `"<marker text>"`, or an identifier (a ktest registry row,
a user test, a host `#[test]`, a harness or script `def`, a harness or script
file). A Rust `<path>::<fn>` is a host `#[test]` or the function a ktest
registry row names; any other function is not found. A bare word that names
none of those but is a Makefile target (`test-e2e-mce`) is that `make`
rule, since ROADMAP's How to read this names
"a `make` target" as a proof without the `make` word. A path that git finds
renamed by a commit between the ticking commit and the head (`git log -M -B`)
resolves at its head path, and a `<path>::<name>` whose Rust module a later
commit split into a directory (`<d>.rs` or `<d>/mod.rs` into `<d>/*.rs`)
resolves in that directory's files. Its definition changes
in `git diff base...head`, or its name appears in the ticked line; otherwise
the line carries `(existing: <reason>)`, which the report lists.

A pushed commit's message cannot change, so a wrong or missing `Proves:`
line is corrected by a row of `tests/gates/proves-errata.toml`, a gate
input: the rules read the row's `proves` in place of `was`, or as the
line when `was` is empty (each non-empty line of `proves`, so a squash
that dropped every trailer can supply them all), the report lists it, and
a row whose commit is in the pull request but does not match that `was`
fails.

A `Fails-before:` line names the tier, the test commit, and the failure
line. That commit must be an earlier commit of the pull request, or a SHA
a later squash dropped from ancestry at which ci-history holds a failed
`pull_request` run of that tier.

Modes:
- bare (`make check`): pairing and the diff rule on `gatelib.pr_diff_base`
  ..HEAD (`BASE_SHA`, or `origin/$GITHUB_BASE_REF` when that is not `main`,
  or `origin/main`), or `check_ticks: skipped (no origin/main)` when no such
  ref exists;
- `--base B [--head H]`: pairing and the diff rule on `B..H`, plus the needs,
  closes and Fails-before rules;
- `--results DIR [--run-commit SHA]`: adds the results, retry and bracket
  rules, reading C-RESULTS files under DIR at the head or at SHA;
- `--summary FILE`: appends a Markdown report to FILE;
- `--commit-msg FILE`: the pairing and proof rules for the commit about to be
  made, from its message FILE and the staged tree (`scripts/hooks/commit-msg`,
  which setup.sh installs), so a bad trailer is refused before it exists.

Errors go to stderr as `<sha7> L<line>: <message>`; the exit code is then 1.
"""

from __future__ import annotations

import argparse
import ast
import json
import re
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402
from tests.harness import registry  # noqa: E402

ROADMAP_PATH = "docs/ROADMAP.md"
# Corrections to pushed `Proves:` lines, which no later commit can edit.
ERRATA_PATH = "tests/gates/proves-errata.toml"
HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
TICKED = re.compile(r"^\s*- \[x\] (.*)$")
EXISTING = re.compile(r"^(.*?)\s+\(existing:\s*(.*)\)$")
IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
MAKE_TARGET = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9_.-]*$")
JOB = re.compile(r"^([A-Za-z0-9_.-]+\.ya?ml):([A-Za-z0-9_-]+)$")
FAILS_BEFORE = re.compile(r"\bthe tests? fails? before the fix\b", re.I)
FAILS_BEFORE_LINE = re.compile(r'^(\S+)\s+([0-9a-f]{7,40})\s+"(.*)"$')
RESULT_KINDS = ("ktest", "utest", "marker")
RUST_FN = r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|async|unsafe|extern\s+\"[^\"]*\")\s+)*fn\s+"
PY_DEF = r"^(\s*)(?:async\s+)?(?:def|class)\s+"
# A ktest registry row: C-SUITES's `test("<name>", f)`, or the legacy tuple
# `("<name>", f),` that starts its own line. A bare `(` would match any call.
REGISTRY_ROW = (r"\btest\(\s*\"{0}\"\s*,\s*([A-Za-z_][A-Za-z0-9_:]*)"
                r"|^[ \t]*\(\s*\"{0}\"\s*,\s*([A-Za-z_][A-Za-z0-9_:]*)\s*\)\s*,")
# `{0}` is the function a row names, so a Rust path::fn can be that row.
REGISTRY_BY_FN = (
    r"\btest\(\s*\"([^\"]+)\"\s*,\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*{0}\b"
    r"|^[ \t]*\(\s*\"([^\"]+)\"\s*,\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*{0}\b")
IGNORE_ATTR = re.compile(r"^\s*#\[ignore\b")
MARKER_CALL = re.compile(r"marker!\(\s*\"((?:[^\"\\]|\\.)*)\"", re.S)
MARKER_CONST = re.compile(r"^\s*pub\s+const\s+([A-Z0-9_]+):\s*&str\s*=\s*\"((?:[^\"\\]|\\.)*)\";",
                          re.M)
# vibeos-core's #[test]s live in crates/core/src/, and in src/cell.rs, which its
# crate root compiles under cfg(test).
HOST_TEST_DIRS = ("src/", "crates/", "tests/hostlib/")
# Where `marker!` calls and the marker constants live.
MARKER_DIRS = ("src/", "crates/core/src/")
MARKER_CONSTS = "crates/core/src/marker.rs"
# The marker registry: a row's text, and a contract row's name (ROADMAP §10.2).
MARKER_REGISTRY = "tests/contract/markers.toml"
PY_DIRS = ("tests/harness/", "scripts/")
HARNESS_DIR = "tests/harness/"
# Workflow file whose jobs a bracket's `make <target>` proof is looked up in.
BRACKET_WORKFLOW = {**gatelib.BRACKETS, "ci-history": "ci.yml"}
WORKFLOW_JOB = re.compile(r"^  ([A-Za-z0-9_-]+):\s*(?:#.*)?$")
WORKFLOW_JOB_NAME = re.compile(r"^    name:\s*(.+?)\s*$")


def collapse(s: str) -> str:
    return " ".join(s.split())


@dataclass
class Tick:
    sha: str
    line: int  # the line in the commit's docs/ROADMAP.md
    text: str  # after the `- [x] ` marker


@dataclass
class Commit:
    sha: str
    message: str = ""
    ticks: list[Tick] = field(default_factory=list)
    # Ticks whose `- [x]` text a later commit of the pull request removes.
    dropped: list[Tick] = field(default_factory=list)


@dataclass
class ProvesLine:
    raw: str
    proof: str
    prefix: str
    bracket: str | None = None
    existing: str | None = None


@dataclass
class Definition:
    kind: str  # make, path, marker, ktest, utest, host, py, file
    path: str
    start: int  # 1-based, inclusive; 0 for a whole file
    end: int
    name: str
    ignored: bool = False  # a host test with `#[ignore]`


@dataclass
class Report:
    errors: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)
    existing: list[str] = field(default_factory=list)
    ticks: int = 0

    def error(self, sha: str, line: int | None, msg: str) -> None:
        where = f" L{line}" if line else ""
        self.errors.append(f"{sha[:7]}{where}: {msg}")


def parse_proves(raw: str, tag: str = "Proves") -> ProvesLine | str:
    """A `Proves:` line's parts, or the reason it is malformed."""
    if " -- " not in raw:
        return f"{tag}: line has no ` -- <prefix>`: {raw!r}"
    proof, prefix = raw.split(" -- ", 1)
    proof, prefix = proof.strip(), prefix.strip()
    existing = None
    m = EXISTING.match(proof)
    if m:
        proof, existing = m.group(1).strip(), m.group(2).strip()
    try:
        bracket = gatelib.parse_bracket(proof)
    except gatelib.GateError as e:
        return f"{tag}: {e}: {raw!r}"
    if bracket is not None:
        proof = proof[: proof.rindex("[")].strip()
    if not proof:
        return f"{tag}: line names no proof: {raw!r}"
    return ProvesLine(raw, proof, prefix, bracket, existing)


def parse_fails_before(raw: str) -> ProvesLine | str:
    """`<tier> <sha> "<failure line>" -- <prefix>`: `proof` holds the part
    before the separator, which `FAILS_BEFORE_LINE` splits."""
    m = re.match(r'^(\S+\s+[0-9a-f]{7,40}\s+"(.*)")\s+--\s+(.*)$', raw.strip())
    if m is None:
        return f"Fails-before: not `<tier> <sha> \"<line>\" -- <prefix>`: {raw!r}"
    if not m.group(2).strip():
        return f"Fails-before: the failure line is empty: {raw!r}"
    return ProvesLine(raw, m.group(1), m.group(3).strip())


def parse_roadmap_diff(diff: str) -> dict[str, tuple[list[tuple[int, str]], list[str]]]:
    """Per commit: the added `- [x]` lines (head line, text) and the removed
    `- [x]` texts of `git log -p -U0` output whose commits start with NUL+SHA."""
    out: dict[str, tuple[list[tuple[int, str]], list[str]]] = {}
    added: list[tuple[int, str]] = []
    removed: list[str] = []
    new_line = 0
    for raw in diff.split("\n"):
        if raw.startswith("\0"):
            added, removed = [], []
            out[raw[1:].strip()] = (added, removed)
            continue
        h = HUNK.match(raw)
        if h:
            new_line = int(h.group(3))
            continue
        if raw.startswith("+++") or raw.startswith("---"):
            continue
        if raw.startswith("+"):
            t = TICKED.match(raw[1:])
            if t:
                added.append((new_line, t.group(1)))
            new_line += 1
        elif raw.startswith("-"):
            t = TICKED.match(raw[1:])
            if t:
                removed.append(t.group(1))
    return out


def parse_changed(diff: str) -> dict[str, list[tuple[int, int]]]:
    """Head-side changed ranges per file of `git diff -U0`: (start, count);
    a pure deletion after line c is (c, 0)."""
    out: dict[str, list[tuple[int, int]]] = {}
    current: list[tuple[int, int]] | None = None
    old_path = ""
    for raw in diff.split("\n"):
        if raw.startswith("--- "):
            old_path = raw[6:] if raw.startswith("--- a/") else ""
            continue
        if raw.startswith("+++ "):
            path = raw[6:] if raw.startswith("+++ b/") else old_path
            current = out.setdefault(path, [])
            continue
        h = HUNK.match(raw)
        if h and current is not None:
            count = 1 if h.group(4) is None else int(h.group(4))
            current.append((int(h.group(3)), count))
    return out


class Tree:
    """Files at one revision, read through git and cached."""

    def __init__(self, rev: str, repo: Path) -> None:
        self.rev = rev
        self.repo = repo
        self._files: list[str] | None = None
        self._text: dict[str, str | None] = {}

    def files(self) -> list[str]:
        if self._files is None:
            out = gatelib.git(self.repo, "ls-tree", "-r", "-z", "--name-only", self.rev)
            self._files = [p for p in out.split("\0") if p]
        return self._files

    def read(self, path: str) -> str | None:
        if path not in self._text:
            try:
                self._text[path] = gatelib.git(self.repo, "show", f"{self.rev}:{path}")
            except gatelib.GateError:
                self._text[path] = None
        return self._text[path]

    def grep(self, word: str, prefixes: tuple[str, ...]) -> list[str]:
        """Files under `prefixes` holding `word` as a whole word."""
        out = gatelib.git(self.repo, "grep", "-l", "-w", "-F", "-e", word, self.rev, "--",
                          *prefixes, check=False)
        pre = f"{self.rev}:"
        return sorted({p[len(pre):] if p.startswith(pre) else p for p in out.splitlines() if p})


def _rust_fn_range(lines: list[str], i: int) -> tuple[int, int]:
    """0-based index `i` of a `fn` line -> 1-based (start, end), from its
    attributes and doc comments to rustfmt's closing `}` at its indent."""
    indent = len(lines[i]) - len(lines[i].lstrip())
    start = i
    while start > 0 and re.match(r"^\s*(#\[|#!\[|///|//!)", lines[start - 1]):
        start -= 1
    end = i
    body = lines[i].rstrip()
    if not (body.endswith(";") or (body.endswith("}") and "{" in body)):
        closing = " " * indent + "}"
        for j in range(i + 1, len(lines)):
            if lines[j].rstrip() == closing:
                end = j
                break
        else:
            end = len(lines) - 1
    return start + 1, end + 1


def _is_ignored(attrs: list[str]) -> bool:
    """An `#[ignore]` or `#[ignore = "..."]` attribute line; a doc comment or
    another attribute that mentions the word does not count."""
    return any(IGNORE_ATTR.match(a) for a in attrs)


def _py_header_end(lines: list[str], i: int) -> int:
    """0-based index of the line that ends the `def`/`class` header at `i`:
    the first line after which its brackets are closed again, so a
    multi-line signature's `) -> T:` line belongs to the header."""
    depth = 0
    for j in range(i, len(lines)):
        code = re.sub(r"'(?:[^'\\]|\\.)*'|\"(?:[^\"\\]|\\.)*\"", "", lines[j])
        code = code.split("#", 1)[0]
        depth += sum(code.count(c) for c in "([{") - sum(code.count(c) for c in ")]}")
        if depth <= 0:
            return j
    return len(lines) - 1


def _py_def_range(lines: list[str], i: int) -> tuple[int, int]:
    """0-based index `i` of a `def` line -> 1-based (start, end), from its
    decorators past its header to the next line at its indent or less."""
    indent = len(lines[i]) - len(lines[i].lstrip())
    start = i
    while start > 0 and lines[start - 1].strip().startswith("@"):
        start -= 1
    end = _py_header_end(lines, i)
    for j in range(end + 1, len(lines)):
        s = lines[j]
        if not s.strip():
            continue
        if len(s) - len(s.lstrip()) <= indent:
            break
        end = j
    return start + 1, end + 1


def find_fn(text: str, name: str, rust: bool) -> list[tuple[int, int, list[str]]]:
    """Each definition of `name` in `text`: (start, end, attribute lines)."""
    pat = re.compile((RUST_FN if rust else PY_DEF) + re.escape(name) + r"\b")
    lines = text.splitlines()
    out: list[tuple[int, int, list[str]]] = []
    for i, raw in enumerate(lines):
        if pat.match(raw):
            s, e = (_rust_fn_range if rust else _py_def_range)(lines, i)
            out.append((s, e, lines[s - 1:i]))
    return out


def _has_test_attr(attrs: list[str]) -> bool:
    return any(a.strip().startswith("#[test]") for a in attrs)


def _ktest_rows_for_fn(fn: str, tree: Tree) -> list[Definition]:
    """Registry rows whose function is `fn`, and that function where the row's
    file defines it, as kind ktest under the row's name."""
    row = re.compile(REGISTRY_BY_FN.format(re.escape(fn)), re.M)
    out: list[Definition] = []
    for path in tree.files():
        if not path.startswith("src/") or not path.endswith(".rs") or "ktest" not in path:
            continue
        src = tree.read(path) or ""
        for m in row.finditer(src):
            name = m.group(1) or m.group(2)
            a = src.count("\n", 0, m.start()) + 1
            b = src.count("\n", 0, m.end()) + 1
            out.append(Definition("ktest", path, a, b, name))
            for s, e, _attrs in find_fn(src, fn, True):
                out.append(Definition("ktest", path, s, e, name))
    return out


def _rust_path_fn(path: str, name: str, text: str, tree: Tree) -> list[Definition]:
    """A Rust `path::fn`: its `#[test]`s, or the ktest rows that name it.
    A production function is not a proof."""
    fns = find_fn(text, name, True)
    if not fns:
        return []
    tested = [(s, e, attrs) for s, e, attrs in fns if _has_test_attr(attrs)]
    if tested:
        return [Definition("host", path, s, e, name, _is_ignored(attrs)) for s, e, attrs in tested]
    rows = _ktest_rows_for_fn(name, tree)
    if not rows:
        return []
    reg = rows[0].name
    for s, e, _attrs in fns:
        if not any(d.path == path and d.start == s for d in rows):
            rows.append(Definition("ktest", path, s, e, reg))
    return rows


def _make_rule(text: str, target: str) -> tuple[int, int] | None:
    lines = text.splitlines()
    pat = re.compile(r"^([^\s:=#][^:=]*?)\s*::?(?!=)")
    for i, raw in enumerate(lines):
        m = pat.match(raw)
        if m is None or target not in m.group(1).split():
            continue
        end = i
        cont = raw.endswith("\\")
        for j in range(i + 1, len(lines)):
            if lines[j].startswith("\t") or cont:
                end = j
                cont = lines[j].endswith("\\")
                continue
            break
        return i + 1, end + 1
    return None


def _marker_regex(fmt: str) -> re.Pattern[str]:
    parts: list[str] = []
    i = 0
    while i < len(fmt):
        if fmt.startswith("{{", i) or fmt.startswith("}}", i):
            parts.append(re.escape(fmt[i]))
            i += 2
        elif fmt[i] == "{":
            j = fmt.find("}", i)
            if j < 0:
                parts.append(re.escape(fmt[i:]))
                break
            parts.append(".*")
            i = j + 1
        else:
            parts.append(re.escape(fmt[i]))
            i += 1
    return re.compile("".join(parts), re.S)


def resolve(proof: str, tree: Tree) -> tuple[list[Definition], str]:
    """The definitions a proof names at the head, and its name for the
    box-text test. No definitions: the proof was not found."""
    if proof.startswith("make "):
        target = proof.split()[1] if len(proof.split()) > 1 else ""
        text = tree.read("Makefile")
        rng = _make_rule(text, target) if text is not None and target else None
        return ([Definition("make", "Makefile", *rng, target)] if rng else []), f"make {target}"
    if len(proof) >= 2 and proof[0] == proof[-1] == '"':
        return _resolve_marker(proof[1:-1], tree), proof[1:-1]
    if "/" in proof:
        path, _, name = proof.partition("::")
        text = tree.read(path)
        if text is None:
            return [], path
        if not name:
            return [Definition("path", path, 0, 0, path)], path
        if path.endswith(".rs"):
            defs = _rust_path_fn(path, name, text, tree)
            return defs, defs[0].name if defs else name
        defs = [Definition("py", path, s, e, name) for s, e, _attrs in find_fn(text, name, False)]
        return defs, name
    j = JOB.match(proof)
    if j:
        return _resolve_job(j.group(1), j.group(2), tree), proof
    if IDENT.match(proof):
        defs = _resolve_ident(proof, tree)
        if defs:
            return defs, proof
    if MAKE_TARGET.match(proof):
        text = tree.read("Makefile")
        rng = _make_rule(text, proof) if text is not None else None
        if rng:
            return [Definition("make", "Makefile", *rng, proof)], proof
    return [], proof


def _resolve_job(workflow: str, job: str, tree: Tree) -> list[Definition]:
    path = f".github/workflows/{workflow}"
    text = tree.read(path)
    if text is None:
        return []
    lines = text.splitlines()
    for i, raw in enumerate(lines):
        if raw == f"  {job}:":
            end = i
            for k in range(i + 1, len(lines)):
                if lines[k].strip() and not lines[k].startswith("   "):
                    break
                end = k
            return [Definition("job", path, i + 1, end + 1, job)]
    return []


def _resolve_marker(text: str, tree: Tree) -> list[Definition]:
    out: list[Definition] = []
    for path in tree.grep("marker", MARKER_DIRS):
        src = tree.read(path) or ""
        for m in MARKER_CALL.finditer(src):
            fmt = m.group(1).encode().decode("unicode_escape")
            if _marker_regex(fmt).fullmatch(text):
                a = src.count("\n", 0, m.start()) + 1
                b = src.count("\n", 0, m.end()) + 1
                out.append(Definition("marker", path, a, b, text))
    consts = tree.read(MARKER_CONSTS) or ""
    for m in MARKER_CONST.finditer(consts):
        if m.group(2) == text:
            n = consts.count("\n", 0, m.start()) + 1
            out.append(Definition("marker", MARKER_CONSTS, n, n, text))
    raw = tree.read(MARKER_REGISTRY) or ""
    for row in _registry_rows(tree):
        if re.fullmatch(registry.row_regex(row.text), text, re.S):
            at = raw.find(f"text = {json.dumps(row.text, ensure_ascii=False)}")
            n = raw.count("\n", 0, at) + 1 if at >= 0 else 0
            out.append(Definition("marker", MARKER_REGISTRY, n, n, text))
    return out


def _registry_rows(tree: Tree) -> tuple[registry.Row, ...]:
    """The marker registry's rows at the tree, or none when it is missing or
    malformed (`check_markers.py` reports that)."""
    raw = tree.read(MARKER_REGISTRY)
    if raw is None:
        return ()
    try:
        return registry.parse_rows(raw, Path(MARKER_REGISTRY))
    except registry.RegistryError:
        return ()


def _fstring_regex(node: ast.expr, hole: str) -> str | None:
    """A str constant or f-string as a regex, each `{...}` read as `hole`."""
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return re.escape(node.value)
    if isinstance(node, ast.JoinedStr):
        parts: list[str] = []
        for v in node.values:
            if isinstance(v, ast.Constant) and isinstance(v.value, str):
                parts.append(re.escape(v.value))
            else:
                parts.append(hole)
        return "".join(parts)
    return None


def marker_labels(text: str, tree: Tree) -> list[re.Pattern[str]]:
    """The C-RESULTS names of a marker proof: the names of the marker
    registry's contract rows whose text matches `text` (each placeholder any
    text), and the labels of the harness's
    `Marker("<needle>", "<label>", and_contains=(...))` entries under
    tests/harness/ (test files left out) whose needle and fragments all occur
    in `text`. The harness records a marker's label, never its text."""
    out: list[re.Pattern[str]] = [
        re.compile(registry.row_regex(row.name))
        for row in _registry_rows(tree)
        if row.kind == "contract" and row.name is not None
        and re.fullmatch(registry.row_regex(row.text), text, re.S)
    ]
    for path in tree.grep("Marker", (HARNESS_DIR,)):
        if not path.endswith(".py") or path.rsplit("/", 1)[-1].startswith("test_"):
            continue
        try:
            mod = ast.parse(tree.read(path) or "")
        except SyntaxError:
            continue
        for node in ast.walk(mod):
            if not isinstance(node, ast.Call):
                continue
            f = node.func
            if (f.id if isinstance(f, ast.Name) else getattr(f, "attr", None)) != "Marker":
                continue
            kw = {k.arg: k.value for k in node.keywords if k.arg}
            args = list(node.args) + [kw[k] for k in ("substring", "name") if k in kw]
            if len(args) < 2:
                continue
            needle, label = _fstring_regex(args[0], ".*"), _fstring_regex(args[1], ".+")
            if needle is None or label is None or not re.search(needle, text, re.S):
                continue
            frags = kw.get("and_contains")
            if isinstance(frags, ast.Tuple | ast.List) and not all(
                    isinstance(e, ast.Constant) and isinstance(e.value, str)
                    and e.value in text for e in frags.elts):
                continue
            out.append(re.compile(label))
    return out


def _resolve_ident(name: str, tree: Tree) -> list[Definition]:
    # A ktest registry row: legacy `("<name>", f)` or C-SUITES's `test("<name>", f)`.
    row = re.compile(REGISTRY_ROW.format(re.escape(name)), re.M)
    out: list[Definition] = []
    for path in [p for p in tree.files() if p.startswith("src/") and p.endswith(".rs")]:
        if "ktest" not in path:
            continue
        src = tree.read(path) or ""
        for m in row.finditer(src):
            a = src.count("\n", 0, m.start()) + 1
            b = src.count("\n", 0, m.end()) + 1
            out.append(Definition("ktest", path, a, b, name))
            fn = (m.group(1) or m.group(2)).rsplit("::", 1)[-1]
            for s, e, _ in find_fn(src, fn, True):
                out.append(Definition("ktest", path, s, e, name))
    if out:
        return out
    for kind, prefixes, rust in (("utest", ("user/",), True), ("host", HOST_TEST_DIRS, True),
                                 ("py", PY_DIRS, False)):
        for path in tree.grep(name, prefixes):
            if rust != path.endswith(".rs") or (not rust and not path.endswith(".py")):
                continue
            for s, e, attrs in find_fn(tree.read(path) or "", name, rust):
                if kind == "host" and not any(a.strip().startswith("#[test]") for a in attrs):
                    continue
                out.append(Definition(kind, path, s, e, name, _is_ignored(attrs)))
        if out:
            return out
    for d in PY_DIRS:
        path = f"{d}{name}.py"
        if tree.read(path) is not None:
            out.append(Definition("file", path, 0, 0, name))
    return out


def _named_in(name: str, text: str) -> bool:
    if not name:
        return False
    if IDENT.match(name):
        return re.search(r"(?<![A-Za-z0-9_])" + re.escape(name) + r"(?![A-Za-z0-9_])",
                         text) is not None
    return name in text


def touched(d: Definition, changed: dict[str, list[tuple[int, int]]]) -> bool:
    ranges = changed.get(d.path)
    if ranges is None:
        return False
    if d.start == 0:
        return True
    for start, count in ranges:
        if count == 0:
            if d.start <= start < d.end:
                return True
        elif start <= d.end and start + count - 1 >= d.start:
            return True
    return False


Run = dict[str, Any]


class Gh:
    """Workflow runs and their results artifacts, read through `gh`."""

    def __init__(self, repo: Path = gatelib.ROOT) -> None:
        self.repo = repo

    def _gh(self, *args: str) -> str:
        try:
            r = subprocess.run(["gh", *args], cwd=self.repo, capture_output=True, text=True,
                               check=False)
        except OSError as e:
            raise gatelib.GateError(f"gh {args[0]}: {e}") from e
        if r.returncode != 0:
            raise gatelib.GateError(f"gh {' '.join(args)}: {r.stderr.strip()}")
        return r.stdout

    def runs(self, workflow_file: str) -> list[Run]:
        out = self._gh("run", "list", "--workflow", workflow_file, "--limit", "200", "--json",
                       "databaseId,headSha,event,conclusion")
        return [{"id": r.get("databaseId"), "head_sha": r.get("headSha"),
                 "event": r.get("event"), "conclusion": r.get("conclusion"),
                 "workflow": workflow_file} for r in json.loads(out or "[]")]

    def jobs(self, run_id: object) -> list[dict[str, Any]]:
        out = self._gh("run", "view", str(run_id), "--json", "jobs")
        jobs = json.loads(out or "{}").get("jobs", [])
        return [{"name": j.get("name"), "conclusion": j.get("conclusion")} for j in jobs]

    def download_results(self, run_id: object, dest: Path) -> list[dict[str, Any]]:
        self._gh("run", "download", str(run_id), "--pattern", "results-*", "--dir", str(dest))
        return gatelib.load_results(dest)


class History:
    """C-HISTORY run and dev-host records on the `ci-history` branch. No
    branch, no records."""

    REF = "refs/remotes/origin/ci-history"

    def __init__(self, repo: Path = gatelib.ROOT) -> None:
        self.repo = repo
        self._records: list[Run] | None = None

    def records(self) -> list[Run]:
        if self._records is None:
            self._records = self._read()
        return self._records

    def _read(self) -> list[Run]:
        gatelib.git(self.repo, "fetch", "-q", "--no-tags", "origin",
                    f"+refs/heads/ci-history:{self.REF}", check=False)
        if not gatelib.git(self.repo, "rev-parse", "--verify", "-q", self.REF,
                           check=False).strip():
            return []
        names = [n for n in gatelib.git(self.repo, "ls-tree", "-r", "-z", "--name-only",
                                        self.REF).split("\0") if n.endswith(".json")]
        batch = "".join(f"{self.REF}:{n}\n" for n in names).encode()
        r = subprocess.run(["git", "-C", str(self.repo), "cat-file", "--batch"], input=batch,
                           capture_output=True, check=False)
        out: list[Run] = []
        data = r.stdout  # `<sha> blob <size>\n<size bytes>\n` per name
        pos = 0
        while pos < len(data):
            nl = data.index(b"\n", pos)
            header = data[pos:nl].split()
            pos = nl + 1
            if len(header) < 3:
                continue
            size = int(header[2])
            blob, pos = data[pos:pos + size], pos + size + 1
            try:
                rec = json.loads(blob.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            if isinstance(rec, dict):
                rec.setdefault("id", rec.get("run_id"))
                out.append(rec)
        return out


def record_results(run: Run) -> list[dict[str, Any]]:
    """The results objects a history record carries, per job or at its top."""
    out = [r for r in run.get("results", []) if isinstance(r, dict)]
    for j in run.get("jobs", []) or []:
        if isinstance(j, dict):
            out.extend(r for r in j.get("results", []) or [] if isinstance(r, dict))
    return out


def dev_host_ran(rec: Run, d: Definition) -> bool:
    """A dev-host record (`gate.py --record`) proves a `make` target or a
    script it ran: its `result` is `pass` and its `command` is `make <target>`
    or names the script's path. It has no `conclusion`, since no workflow
    ran it."""
    cmd = rec.get("command")
    if rec.get("result") != "pass" or not isinstance(cmd, str):
        return False
    words = cmd.split()
    if d.kind == "make":
        return words[:2] == ["make", d.name]
    if d.kind == "path":
        return d.path in words
    return False


def passed_in(results: list[dict[str, Any]], kind: str, name: str,
              labels: list[re.Pattern[str]] | None = None) -> bool:
    """`name`, or a name one of `labels` fully matches, is in a `passed` list."""
    for r in results:
        sect = r.get(kind)
        if not isinstance(sect, dict):
            continue
        passed = [p for p in sect.get("passed") or [] if isinstance(p, str)]
        if name in passed or any(lb.fullmatch(p) for lb in labels or [] for p in passed):
            return True
    return False


def workflow_jobs(text: str) -> list[tuple[str, str, list[str]]]:
    """The jobs of a workflow file: (id, `name:` or the id, body lines)."""
    out: list[tuple[str, str, list[str]]] = []
    in_jobs = False
    for raw in text.splitlines():
        if raw and not raw[0].isspace() and not raw.startswith("#"):
            in_jobs = raw.rstrip() == "jobs:"
            continue
        if not in_jobs:
            continue
        m = WORKFLOW_JOB.match(raw)
        if m:
            out.append((m.group(1), m.group(1), []))
            continue
        if out:
            out[-1][2].append(raw)
            n = WORKFLOW_JOB_NAME.match(raw)
            if n and out[-1][1] == out[-1][0]:
                out[-1] = (out[-1][0], n.group(1).strip("'\""), out[-1][2])
    return out


def jobs_running(text: str, target: str) -> list[tuple[str, str]]:
    """(id, name) of each job of a workflow whose steps run `make <target>`;
    the name is what a run's job list shows for it."""
    pat = re.compile(r"(?<![\w-])make\s+(?:[^\n;&|#]*?\s)?" + re.escape(target) + r"(?![\w-])")
    return [(jid, name) for jid, name, body in workflow_jobs(text)
            if any(pat.search(line.split("#", 1)[0]) for line in body)]


def _job_name_regex(key: str) -> re.Pattern[str]:
    """A run's name for a job: `key`, `${{ ... }}` rendered as any text, and a
    matrix job's ` (<values>)` suffix."""
    parts = re.split(r"\$\{\{.*?\}\}", key)
    return re.compile(".+".join(re.escape(x) for x in parts) + r"(?: \(.*\))?")


class Checker:
    """The rules of one run over `base..head`."""

    def __init__(self, base: str, head: str, repo: Path, *, full: bool = True,
                 results_dir: Path | None = None, run_commit: str | None = None,
                 gh: Gh | None = None, history: History | None = None) -> None:
        self.repo = repo
        self.base = base
        self.full = full
        self.head = gatelib.git(repo, "rev-parse", "--verify", f"{head}^{{commit}}").strip()
        self.merge_base = gatelib.git(repo, "merge-base", base, self.head).strip()
        self.pr = gatelib.pr_commits(base, self.head, repo)
        self.tree = Tree(self.head, repo)
        self.report = Report()
        self.commits = self._read_commits()
        self.errata = self._read_errata()
        self._changed: dict[str, list[tuple[int, int]]] | None = None
        self._moved: dict[str, list[tuple[str, str]]] = {}
        self.gh = gh if gh is not None else Gh(repo)
        self.history = history if history is not None else History(repo)
        self.results: list[dict[str, Any]] | None = None
        if results_dir is not None:
            commits = {self.head}
            if run_commit:
                commits.add(gatelib.git(repo, "rev-parse", "--verify", "-q", run_commit,
                                        check=False).strip() or run_commit)
            self.results = [r for r in gatelib.load_results(results_dir)
                            if r.get("commit") in commits and r.get("dirty") is False]
            if not self.results:
                self.report.notes.append(f"no results file at the head in {results_dir}")

    def _read_errata(self) -> dict[tuple[str, str], tuple[str, str]]:
        """ERRATA_PATH's rows at the head, keyed by (commit, the pushed line)."""
        text = self.tree.read(ERRATA_PATH)
        if text is None:
            return {}
        rows, errors = load_errata(text)
        for e in errors:
            self.report.error("errata", None, e)
        return rows

    def _read_commits(self) -> list[Commit]:
        commits = {sha: Commit(sha) for sha in self.pr}
        if not commits:
            return []
        rng = f"{self.base}..{self.head}"
        log = gatelib.git(self.repo, "log", "--no-merges", "--reverse", "--format=%x00%H%n%B",
                          rng)
        for chunk in log.split("\0")[1:]:
            sha, _, msg = chunk.partition("\n")
            if sha in commits:
                commits[sha].message = msg
        diff = gatelib.git(self.repo, "log", "--no-merges", "--reverse", "--full-history",
                           "-p", "-U0", "--no-color", "--no-ext-diff", "--no-renames",
                           "--format=%x00%H", rng, "--", ROADMAP_PATH)
        base_text = gatelib.git(self.repo, "show", f"{self.merge_base}:{ROADMAP_PATH}",
                                check=False)
        at_base = {b.text for b in gatelib.parse_boxes(base_text) if b.ticked}
        parsed = parse_roadmap_diff(diff)
        # Texts a later commit removes. Same-commit removal stays a non-tick
        # (`gone`), not a dropped tick a Proves line may excuse.
        removed_after: dict[str, set[str]] = {}
        later: set[str] = set()
        for sha in reversed(self.pr):
            removed_after[sha] = set(later)
            later.update(parsed.get(sha, ([], []))[1])
        for sha, (added, removed) in parsed.items():
            c = commits.get(sha)
            if c is None:
                continue
            gone = set(removed)
            withdrawn = removed_after.get(sha, set())
            for n, t in added:
                if t in gone or t in at_base:
                    continue
                tick = Tick(sha, n, t)
                if t in withdrawn:
                    c.dropped.append(tick)
                else:
                    c.ticks.append(tick)
        return [commits[s] for s in self.pr]

    def changed(self) -> dict[str, list[tuple[int, int]]]:
        if self._changed is None:
            diff = gatelib.git(self.repo, "diff", "-U0", "--no-color", "--no-ext-diff",
                               "--no-renames", f"{self.merge_base}...{self.head}")
            self._changed = parse_changed(diff)
        return self._changed

    def pair(self, c: Commit, tag: str) -> list[tuple[ProvesLine, Tick]]:
        """Pair each `<tag>:` line of `c` with the one tick its prefix begins."""
        out: list[tuple[ProvesLine, Tick]] = []
        lines = list(gatelib.parse_message_lines(c.message, tag))
        if tag == "Proves" and (c.sha, "") in self.errata and not lines:
            raw, why = self.errata[(c.sha, "")]
            lines = [ln.strip() for ln in raw.splitlines() if ln.strip()]
            for item in lines:
                self.report.notes.append(f"{c.sha[:7]}: erratum: Proves: {item} ({why})")
        for raw in lines:
            if tag == "Proves" and raw and (c.sha, raw) in self.errata:
                raw, why = self.errata[(c.sha, raw)]
                self.report.notes.append(f"{c.sha[:7]}: erratum: Proves: {raw} ({why})")
            p = parse_proves(raw) if tag == "Proves" else parse_fails_before(raw)
            if isinstance(p, str):
                self.report.error(c.sha, None, p)
                continue
            pre = collapse(p.prefix)
            hits = [t for t in c.ticks if collapse(t.text).startswith(pre)]
            if not hits:
                dropped = [t for t in c.dropped if collapse(t.text).startswith(pre)]
                if len(dropped) == 1:
                    self.report.notes.append(f"{c.sha[:7]}: {tag}: line the pull request "
                                             f"removes: {p.prefix!r}")
                    continue
                self.report.error(c.sha, None, f"{tag}: pairs with no line this commit "
                                  f"ticks: {p.prefix!r}")
            elif len(hits) > 1:
                where = ", ".join(f"L{t.line}" for t in hits)
                self.report.error(c.sha, None, f"{tag}: prefix pairs with {len(hits)} "
                                  f"ticked lines ({where}): {p.prefix!r}")
            else:
                out.append((p, hits[0]))
        return out

    def check_commit(self, c: Commit) -> None:
        pairs = self.pair(c, "Proves")
        paired = {id(t) for _, t in pairs}
        for t in c.ticks:
            if id(t) not in paired:
                self.report.error(c.sha, t.line, "ticked with no `Proves:` line: "
                                  + " ".join(t.text.split()[:8]))
        for p, t in pairs:
            self.check_proof(c, p, t)
        if self.full:
            self.check_fails_before(c)
            self.check_closes(c)

    def moved(self, sha: str, path: str) -> str | None:
        """Where `path` of commit `sha` is at the head: each later commit's
        renames applied in order (`-B` pairs a path a new file reuses with its
        old content), or None when no commit renamed it."""
        if sha not in self._moved:
            out = gatelib.git(self.repo, "log", "--reverse", "--topo-order", "-M", "-B",
                              "--name-status", "--no-color", "--no-ext-diff", "--format=",
                              "-z", f"{sha}..{self.head}")
            f = out.split("\0")
            renames: list[tuple[str, str]] = []
            i = 0
            while i < len(f):
                status = f[i].strip()
                if not status:
                    i += 1
                elif status.startswith(("R", "C")):
                    renames.append((f[i + 1], f[i + 2]))
                    i += 3
                else:
                    i += 2
            self._moved[sha] = renames
        now, hit = path, False
        for old, new in self._moved[sha]:
            if old == now:
                now, hit = new, True
        return now if hit else None

    def in_split(self, path: str, name: str) -> list[Definition]:
        """`name`'s definitions in the files a split of the Rust module at `path`
        left: the `.rs` files directly in `<d>/`, for a `path` of `<d>.rs` or
        `<d>/mod.rs`."""
        if not path.endswith(".rs"):
            return []
        d = path[:-len("/mod.rs")] if path.endswith("/mod.rs") else path[:-len(".rs")]
        out: list[Definition] = []
        for f in self.tree.files():
            rel = f[len(d) + 1:] if f.startswith(d + "/") else ""
            if rel.endswith(".rs") and "/" not in rel:
                out.extend(resolve(f"{f}::{name}", self.tree)[0])
        return out

    def check_proof(self, c: Commit, p: ProvesLine, t: Tick) -> list[Definition]:
        defs, name = resolve(p.proof, self.tree)
        path, sep, rest = p.proof.partition("::")
        if not defs and "/" in path and not p.proof.startswith('"'):
            # A path proof whose file a later commit moved resolves at its new path.
            new = self.moved(c.sha, path)
            if new is not None:
                defs, name = resolve(new + sep + rest, self.tree)
            if not defs and rest:
                # A Rust module a later commit split into a directory keeps its items
                # in that directory's files (ROADMAP §10.3, Q5).
                defs = self.in_split(path if new is None else new, rest)
        if not defs:
            self.report.error(c.sha, t.line, f"proof not found at the head: {p.proof!r}")
            return defs
        if p.existing is not None:
            self.report.existing.append(f"{c.sha[:7]} L{t.line}: {p.proof} "
                                        f"(existing: {p.existing})")
        elif not (_named_in(name, t.text) or any(touched(d, self.changed()) for d in defs)):
            self.report.error(c.sha, t.line, f"proof {p.proof!r} is not changed by the pull "
                              "request and not named in the box; mark it `(existing: <reason>)`")
        hosts = [d for d in defs if d.kind == "host"]
        if hosts and all(d.ignored for d in hosts):
            self.report.error(c.sha, t.line, f"host test {p.proof!r} is #[ignore]d")
        if p.bracket is not None:
            if self.results is not None:
                self.check_bracket(c, p, t, defs)
        elif self.results is not None:
            kind = defs[0].kind
            if kind in RESULT_KINDS and not passed_in(self.results, kind, defs[0].name,
                                                      self.labels(defs[0])):
                self.report.error(c.sha, t.line, f"{kind} {defs[0].name!r} passed in no "
                                  "results file at the head")
        return defs

    # Bracketed proofs (C-TICK).

    def _runs(self, bracket: str) -> list[tuple[Run, bool]]:
        """Candidate runs for a bracket, each with whether it is a history
        record (whose results it carries) rather than a `gh` run."""
        out: list[tuple[Run, bool]] = []
        recs = self.history.records()
        if bracket == "dev-host":
            return [(r, True) for r in recs if r.get("event") == "dev-host"]
        if bracket == "ci-history":
            return [(r, True) for r in recs if _workflow_is(r, "ci.yml", self.tree)
                    and r.get("branch") == "main"]
        wf = gatelib.BRACKETS[bracket] or ""
        out.extend((r, True) for r in recs if _workflow_is(r, wf, self.tree))
        if bracket != "release":
            out.extend((r, False) for r in self.gh.runs(wf))
        return out

    def _counts(self, run: Run, dev_host: bool) -> bool:
        if dev_host:
            return gatelib.commit_counts_for_pr(gatelib.run_commit(run), self.pr, self.head,
                                                merge_base=self.merge_base, repo=self.repo)
        return gatelib.run_counts_for_pr(run, self.pr, self.head, merge_base=self.merge_base,
                                         repo=self.repo)

    def check_bracket(self, c: Commit, p: ProvesLine, t: Tick, defs: list[Definition]) -> None:
        bracket = p.bracket or ""
        d = defs[0]
        try:
            runs = self._runs(bracket)
            for run, is_record in runs:
                if not self._counts(run, bracket == "dev-host"):
                    continue
                if self._run_passes(run, is_record, d, bracket):
                    where = (f"the dev-host record at {gatelib.run_commit(run)[:12]}"
                             if bracket == "dev-host" else f"run {run.get('id')}")
                    self.report.notes.append(f"{c.sha[:7]} L{t.line}: {p.proof} [{bracket}] "
                                             f"passed in {where}")
                    return
        except gatelib.GateError as e:
            self.report.error(c.sha, t.line, f"[{bracket}] runs unreadable: {e}")
            return
        self.report.error(c.sha, t.line, f"{p.proof} [{bracket}]: no run at the head, a "
                          "docs-only commit of the pull request, or a docs-only merge base "
                          "shows it passed; the box stays open until one does (Tick-post)")

    def labels(self, d: Definition) -> list[re.Pattern[str]]:
        """The results names a marker proof passes under (its harness labels)."""
        return marker_labels(d.name, self.tree) if d.kind == "marker" else []

    def _run_passes(self, run: Run, is_record: bool, d: Definition, bracket: str) -> bool:
        if d.kind in RESULT_KINDS:
            if is_record:
                results = record_results(run)
            else:
                with tempfile.TemporaryDirectory() as tmp:
                    results = self.gh.download_results(run.get("id"), Path(tmp))
            return passed_in(results, d.kind, d.name, self.labels(d))
        if bracket == "dev-host":
            return dev_host_ran(run, d)
        wf = BRACKET_WORKFLOW.get(bracket)
        if d.kind not in ("job", "make") or (d.kind == "make" and wf is None):
            return run.get("conclusion") == "success"
        jobs = run.get("jobs") if is_record else self.gh.jobs(run.get("id"))
        concl = [(str(j.get("name") or ""), j.get("conclusion")) for j in jobs or []
                 if isinstance(j, dict)]
        if d.kind == "job":
            return any(n == d.name and c == "success" for n, c in concl)
        # A bracketed `make` target passes when the jobs of the run that run it
        # concluded `success`, whatever the run's own conclusion (#97, D-23).
        keys = [_job_name_regex(name) for _, name in
                jobs_running(self.tree.read(f".github/workflows/{wf}") or "", d.name)]
        hits = [c for n, c in concl if any(k.fullmatch(n) for k in keys)]
        return bool(hits) and all(c == "success" for c in hits)

    # Needs, closes, and Fails-before.

    def _needs_at(self, rev: str) -> tuple[list[gatelib.Box], list[str], list[gatelib.NeedsFile]]:
        text = gatelib.git(self.repo, "show", f"{rev}:{ROADMAP_PATH}", check=False)
        files = gatelib.git(self.repo, "ls-tree", "-z", "--name-only", rev, "tests/gates/",
                            check=False)
        needs: list[gatelib.NeedsFile] = []
        for path in sorted(p for p in files.split("\0") if p):
            m = gatelib.NEEDS_FILE.match(path.rsplit("/", 1)[-1])
            if m is None:
                continue
            body = gatelib.git(self.repo, "show", f"{rev}:{path}")
            rows, roots = gatelib.parse_needs(body, path)
            needs.append((int(m.group(1)), rows, roots))
        return gatelib.parse_boxes(text), text.splitlines(), needs

    def check_needs(self) -> None:
        boxes, lines, needs = self._needs_at(self.head)
        by_text = {b.text: b for b in boxes if b.ticked}
        last: dict[str, str] = {}
        for c in self.commits:
            for t in c.ticks:
                last[t.text] = c.sha
        for text, sha in last.items():
            b = by_text.get(text)
            if b is None:
                continue
            for _, rows, _ in needs:
                for r in rows:
                    try:
                        if gatelib.match_key(r.key, boxes, lines).line != b.line:
                            continue
                    except gatelib.GateError:
                        continue
                    for n in r.needs:
                        try:
                            nb = gatelib.match_key(n, boxes, lines)
                        except gatelib.GateError as e:
                            self.report.error(sha, b.line, f"needs row: {e}")
                            continue
                        if not nb.ticked:
                            self.report.error(sha, b.line, f"needs L{nb.line}, open at the "
                                              f"head: {' '.join(nb.text.split()[:8])}")

    def check_closes(self, c: Commit) -> None:
        if not c.ticks:
            return
        boxes, lines, needs = self._needs_at(c.sha)
        ticked = {t.line for t in c.ticks}
        for _, rows, _ in needs:
            for r in rows:
                for entry in r.closes:
                    try:
                        pair = (gatelib.match_key(r.key, boxes, lines),
                                gatelib.match_key(entry, boxes, lines))
                    except gatelib.GateError:
                        continue
                    if not ({pair[0].line, pair[1].line} & ticked):
                        continue
                    for b in pair:
                        if not b.ticked:
                            self.report.error(c.sha, b.line, "a closes pair ticks together; "
                                              f"L{b.line} stays open: "
                                              + " ".join(b.text.split()[:8]))

    def check_fails_before(self, c: Commit) -> None:
        pairs = self.pair(c, "Fails-before")
        have = {id(t) for _, t in pairs}
        for t in c.ticks:
            if FAILS_BEFORE.search(t.text) and id(t) not in have:
                self.report.error(c.sha, t.line, "the box's test fails before the fix, and the "
                                  "commit carries no `Fails-before:` line for it")
        pr = set(self.pr)
        for p, t in pairs:
            m = FAILS_BEFORE_LINE.match(p.proof)
            if m is None:
                continue
            tier, short, _ = m.groups()
            if _make_rule(self.tree.read("Makefile") or "", tier) is None:
                self.report.error(c.sha, t.line, f"Fails-before: no Makefile rule {tier!r}")
            full = gatelib.git(self.repo, "rev-parse", "--verify", "-q", f"{short}^{{commit}}",
                               check=False).strip()
            sha = full or short
            if full == c.sha:
                self.report.error(c.sha, t.line, f"Fails-before: {short} is not an earlier "
                                  "commit of the pull request")
                continue
            if full not in pr:
                # Squash of a feature PR drops the test-only commit from
                # base..head. ci-history still holds the failed run.
                if self.failed_history_run(tier, sha):
                    self.report.notes.append(
                        f"{c.sha[:7]}: Fails-before {short} is not an earlier commit "
                        f"of this pull request; ci-history has a failed {tier} run "
                        "there")
                else:
                    self.report.error(c.sha, t.line, f"Fails-before: {short} is not an "
                                      "earlier commit of the pull request")
                    continue
            elif subprocess.run(["git", "-C", str(self.repo), "merge-base", "--is-ancestor",
                                 full, c.sha], check=False).returncode != 0:
                self.report.error(c.sha, t.line, f"Fails-before: {short} is not an ancestor "
                                  "of the commit")
                continue
            self.check_fails_history(c, t, tier, sha)

    def failed_history_run(self, tier: str, sha: str) -> bool:
        """A pull_request ci-history record at `sha` failed `tier`."""
        for r in self.history.records():
            if r.get("event") != "pull_request" or r.get("head_sha") != sha:
                continue
            for res in record_results(r):
                if res.get("tier") != tier:
                    continue
                if any((res.get(k) or {}).get("failed") for k in RESULT_KINDS):
                    return True
        return False

    def check_fails_history(self, c: Commit, t: Tick, tier: str, sha: str) -> None:
        if not any(r.get("event") == "pull_request" for r in self.history.records()):
            note = ("Fails-before history clause inert: ci-history holds no pull_request "
                    "record (#93 §9.2 D-01)")
            if note not in self.report.notes:
                self.report.notes.append(note)
            return
        if not self.failed_history_run(tier, sha):
            self.report.error(c.sha, t.line, f"Fails-before: ci-history holds no failed "
                              f"{tier} run at {sha[:7]}")

    def check_retries(self) -> None:
        if self.results is None or not self.report.ticks:
            return
        for r in self.results:
            for x in r.get("retries") or []:
                label = x.get("label", "?") if isinstance(x, dict) else "?"
                line = x.get("failure_line", "") if isinstance(x, dict) else str(x)
                self.report.errors.append(f"{self.head[:7]}: {r.get('tier')} retried "
                                          f"{label}: {line}")

    def run(self) -> Report:
        for c in self.commits:
            self.report.ticks += len(c.ticks)
            self.check_commit(c)
        self.check_errata()
        if self.full:
            self.check_needs()
            if not any(r.get("event") == "pull_request" for r in self.history.records()):
                note = ("Fails-before history clause inert: ci-history holds no pull_request "
                        "record (#93 §9.2 D-01)")
                if note not in self.report.notes:
                    self.report.notes.append(note)
        self.check_retries()
        return self.report


    def check_errata(self) -> None:
        """A row whose commit is in the pull request replaces one of its lines,
        or supplies the line when `was` is empty."""
        by_sha = {c.sha: c for c in self.commits}
        for sha, was in self.errata:
            c = by_sha.get(sha)
            if c is None:
                continue
            existing = gatelib.parse_message_lines(c.message, "Proves")
            if was == "":
                if existing:
                    self.report.error(sha, None, "erratum supplies a `Proves:` line but "
                                      "the commit already has one")
            elif was not in existing:
                self.report.error(sha, None, f"erratum names no `Proves:` line of the "
                                  f"commit: {was!r}")


def load_errata(text: str) -> tuple[dict[tuple[str, str], tuple[str, str]], list[str]]:
    """ERRATA_PATH's `[[erratum]]` rows: `commit` (40 hex digits), `was` (the
    pushed line after `Proves: `, empty when the commit has none), `proves`
    (the line read in its place) and `why`, keyed by (commit, was); and the
    reasons malformed rows were refused."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        return {}, [f"{ERRATA_PATH}: {e}"]
    rows: dict[tuple[str, str], tuple[str, str]] = {}
    errors: list[str] = []
    for i, row in enumerate(data.get("erratum") or []):
        keys = ("commit", "was", "proves", "why")
        if not isinstance(row, dict) or not all(isinstance(row.get(k), str) for k in keys):
            errors.append(f"{ERRATA_PATH}: erratum {i + 1} needs {', '.join(keys)}")
            continue
        if not row["commit"].strip() or not row["proves"].strip() or not row["why"].strip():
            errors.append(f"{ERRATA_PATH}: erratum {i + 1} needs {', '.join(keys)}")
            continue
        if not re.fullmatch(r"[0-9a-f]{40}", row["commit"]):
            errors.append(f"{ERRATA_PATH}: erratum {i + 1}: commit is not a full sha")
            continue
        key = (row["commit"], row["was"].strip())
        if key in rows:
            errors.append(f"{ERRATA_PATH}: erratum {i + 1} repeats a line")
            continue
        rows[key] = (row["proves"].strip(), row["why"].strip())
    return rows, errors


def _workflow_is(run: Run, workflow_file: str, tree: Tree) -> bool:
    """A run or record names `workflow_file` by its file, stem, or `name:`."""
    w = str(run.get("workflow") or "")
    stem = workflow_file.rsplit(".", 1)[0]
    names = {workflow_file, stem, f".github/workflows/{workflow_file}"}
    text = tree.read(f".github/workflows/{workflow_file}") or ""
    m = re.search(r"^name:\s*(.+?)\s*$", text, re.M)
    if m:
        names.add(m.group(1).strip("'\""))
    return w in names


def check(
    base: str,
    head: str,
    *,
    results_dir: Path | None = None,
    run_commit: str | None = None,
    gh: Gh | None = None,
    history: History | None = None,
    repo: Path = gatelib.ROOT,
    full: bool = True,
) -> Report:
    """Every rule over `base..head`; `full=False` keeps pairing and the diff
    rule (and, with `results_dir`, the results rules) only."""
    return Checker(base, head, repo, full=full, results_dir=results_dir, run_commit=run_commit,
                   gh=gh, history=history).run()


def check_message(message: str, repo: Path = gatelib.ROOT) -> Report:
    """The pairing and proof rules for the commit about to be made: `message`
    and the staged tree, against HEAD's ticks and, for the diff rule, the
    merge base with origin/main (HEAD when that ref is missing)."""
    message = "\n".join(ln for ln in message.splitlines() if not ln.startswith("#"))
    ck = Checker.__new__(Checker)
    ck.repo, ck.full, ck.results, ck.report = repo, False, None, Report()
    ck.errata = {}
    ck.tree = Tree(gatelib.git(repo, "write-tree").strip(), repo)
    main = gatelib.git(repo, "rev-parse", "--verify", "-q", "origin/main", check=False).strip()
    base = gatelib.git(repo, "merge-base", main, "HEAD", check=False).strip() if main else ""
    ck._changed = parse_changed(gatelib.git(repo, "diff", "--cached", "-U0", "--no-color",
                                            "--no-ext-diff", "--no-renames", base or "HEAD"))
    diff = gatelib.git(repo, "diff", "--cached", "-U0", "--no-color", "--no-ext-diff",
                       "--no-renames", "--", ROADMAP_PATH)
    added, removed = parse_roadmap_diff("\0staged\n" + diff).get("staged", ([], []))
    head_text = gatelib.git(repo, "show", f"HEAD:{ROADMAP_PATH}", check=False)
    at_head = {b.text for b in gatelib.parse_boxes(head_text) if b.ticked}
    gone = set(removed)
    c = Commit("staged", message)
    c.ticks = [Tick(c.sha, n, x) for n, x in added if x not in gone and x not in at_head]
    # No commit follows the one being made, so no later rename moves its paths.
    ck.head, ck._moved = "HEAD", {c.sha: []}
    ck.report.ticks = len(c.ticks)
    ck.check_commit(c)
    return ck.report


def summary(r: Report, base: str, head: str) -> str:
    out = [f"## check_ticks: {base[:12]}..{head[:12]}", "",
           f"{r.ticks} ticked lines; {'ok' if not r.errors else f'{len(r.errors)} errors'}."]
    for title, items in (("Errors", r.errors), ("Existing proofs", r.existing),
                         ("Notes", r.notes)):
        if items:
            out += ["", f"### {title}", ""] + [f"- {x}" for x in items]
    return "\n".join(out) + "\n"


def print_report(r: Report) -> None:
    for line in r.existing:
        print(f"check_ticks: existing proof: {line}")
    for line in r.notes:
        print(f"check_ticks: {line}")
    if r.errors:
        print("\n".join(r.errors), file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--base", help="the pull request's base (default: origin/main, bare mode)")
    ap.add_argument("--head", default="HEAD", help="the pull request's head (default: HEAD)")
    ap.add_argument("--results", type=Path, help="results files (C-RESULTS) to read")
    ap.add_argument("--run-commit", help="the commit a pull_request run tested (GITHUB_SHA)")
    ap.add_argument("--summary", type=Path, help="append a Markdown report to this file")
    ap.add_argument("--commit-msg", type=Path,
                    help="check the commit about to be made (the commit-msg hook)")
    args = ap.parse_args(argv)
    if args.commit_msg is not None:
        try:
            r = check_message(args.commit_msg.read_text(encoding="utf-8"), repo=ROOT)
        except (OSError, gatelib.GateError) as e:
            print(f"check_ticks: {e}", file=sys.stderr)
            return 1
        print_report(r)
        if r.errors:
            print("check_ticks: this commit's `Proves:` lines do not pair (see above); fix the "
                  "message, since a pushed message cannot be fixed", file=sys.stderr)
            return 1
        return 0
    base = args.base
    full = base is not None
    if base is None:
        base = gatelib.pr_diff_base(ROOT)
        if base is None:
            print("check_ticks: skipped (no origin/main)")
            return 0
    try:
        r = check(base, args.head, results_dir=args.results, run_commit=args.run_commit,
                  repo=ROOT, full=full)
    except gatelib.GateError as e:
        print(f"check_ticks: {e}", file=sys.stderr)
        return 1
    print_report(r)
    if args.summary is not None:
        with args.summary.open("a", encoding="utf-8") as f:
            f.write(summary(r, base, args.head))
    if r.errors:
        return 1
    print(f"check_ticks: ok ({r.ticks} ticks in {base}..{args.head})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
