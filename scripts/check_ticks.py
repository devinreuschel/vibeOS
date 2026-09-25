#!/usr/bin/env python3
"""Every ticked ROADMAP box names its proof (ROADMAP §10.9, the check_ticks.py box).

For each non-merge commit of a pull request (`base..head`), the lines of
docs/ROADMAP.md that the commit changes to `- [x]` are its ticks: an added
`- [x]` line, unless the same commit removes a `- [x]` line with the same text
(a move) or that text is already `- [x]` at the merge base. So a ticked box
whose text changes names its proof again; one reopened or deleted needs none.

Each tick pairs with one `Proves: <proof>[ [<bracket>]][ (existing: <reason>)]
-- <prefix>` line of the commit's message, read anywhere in the message: the
prefix, at least five words, begins that ticked line and no other line the
commit ticks (whitespace collapsed). The line splits at its first ` -- `.

The proof exists at the head: a `make <target>` rule, a path (optionally
`<path>::<name>`), a `"<marker text>"`, or an identifier (a ktest registry row,
a user test, a host `#[test]`, a harness or script `def`, a harness or script
file). Its definition changes in `git diff base...head`, or its name appears in
the ticked line; otherwise the line carries `(existing: <reason>)`, which the
report lists.

Modes:
- bare (`make check`): pairing and the diff rule on `origin/main..HEAD`, or
  `check_ticks: skipped (no origin/main)` when that ref is missing;
- `--base B [--head H]`: the same on `B..H`.

Errors go to stderr as `<sha7> L<line>: <message>`; the exit code is then 1.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402

ROADMAP_PATH = "docs/ROADMAP.md"
MIN_PREFIX_WORDS = 5
HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
TICKED = re.compile(r"^\s*- \[x\] (.*)$")
EXISTING = re.compile(r"^(.*?)\s+\(existing:\s*(.*)\)$")
IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
RUST_FN = r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|async|unsafe|extern\s+\"[^\"]*\")\s+)*fn\s+"
PY_DEF = r"^(\s*)(?:async\s+)?(?:def|class)\s+"
REGISTRY_ROW = r"(?:\btest\(|\()\s*\"{}\"\s*,\s*([A-Za-z_][A-Za-z0-9_:]*)"
MARKER_CALL = re.compile(r"marker!\(\s*\"((?:[^\"\\]|\\.)*)\"", re.S)
MARKER_CONST = re.compile(r"^\s*pub\s+const\s+([A-Z0-9_]+):\s*&str\s*=\s*\"((?:[^\"\\]|\\.)*)\";",
                          re.M)
HOST_TEST_DIRS = ("crates/", "tests/hostlib/")
PY_DIRS = ("tests/harness/", "scripts/")


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


def _py_def_range(lines: list[str], i: int) -> tuple[int, int]:
    indent = len(lines[i]) - len(lines[i].lstrip())
    start = i
    while start > 0 and lines[start - 1].strip().startswith("@"):
        start -= 1
    end = i
    for j in range(i + 1, len(lines)):
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
        rust = path.endswith(".rs")
        defs = [Definition("host" if rust else "py", path, s, e, name,
                           any("ignore" in a for a in attrs))
                for s, e, attrs in find_fn(text, name, rust)]
        return defs, name
    if IDENT.match(proof):
        return _resolve_ident(proof, tree), proof
    return [], proof


def _resolve_marker(text: str, tree: Tree) -> list[Definition]:
    out: list[Definition] = []
    for path in tree.grep("marker", ("src/",)):
        src = tree.read(path) or ""
        for m in MARKER_CALL.finditer(src):
            fmt = m.group(1).encode().decode("unicode_escape")
            if _marker_regex(fmt).fullmatch(text):
                a = src.count("\n", 0, m.start()) + 1
                b = src.count("\n", 0, m.end()) + 1
                out.append(Definition("marker", path, a, b, text))
    consts = tree.read("src/marker.rs") or ""
    for m in MARKER_CONST.finditer(consts):
        if m.group(2) == text:
            n = consts.count("\n", 0, m.start()) + 1
            out.append(Definition("marker", "src/marker.rs", n, n, text))
    return out


def _resolve_ident(name: str, tree: Tree) -> list[Definition]:
    # A ktest registry row: legacy `("<name>", f)` or C-SUITES's `test("<name>", f)`.
    row = re.compile(REGISTRY_ROW.format(re.escape(name)), re.S)
    out: list[Definition] = []
    for path in [p for p in tree.files() if p.startswith("src/") and p.endswith(".rs")]:
        if "ktest" not in path:
            continue
        src = tree.read(path) or ""
        for m in row.finditer(src):
            a = src.count("\n", 0, m.start()) + 1
            b = src.count("\n", 0, m.end()) + 1
            out.append(Definition("ktest", path, a, b, name))
            fn = m.group(1).rsplit("::", 1)[-1]
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
                ignored = any("ignore" in a for a in attrs)
                out.append(Definition(kind, path, s, e, name, ignored))
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


class Checker:
    """The rules of one run over `base..head`."""

    def __init__(self, base: str, head: str, repo: Path) -> None:
        self.repo = repo
        self.base = base
        self.head = gatelib.git(repo, "rev-parse", "--verify", f"{head}^{{commit}}").strip()
        self.merge_base = gatelib.git(repo, "merge-base", base, self.head).strip()
        self.pr = gatelib.pr_commits(base, self.head, repo)
        self.tree = Tree(self.head, repo)
        self.report = Report()
        self.commits = self._read_commits()
        self._changed: dict[str, list[tuple[int, int]]] | None = None

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
        for sha, (added, removed) in parse_roadmap_diff(diff).items():
            c = commits.get(sha)
            if c is None:
                continue
            gone = set(removed)
            c.ticks = [Tick(sha, n, t) for n, t in added if t not in gone and t not in at_base]
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
        for raw in gatelib.parse_message_lines(c.message, tag):
            p = parse_proves(raw, tag)
            if isinstance(p, str):
                self.report.error(c.sha, None, p)
                continue
            if len(p.prefix.split()) < MIN_PREFIX_WORDS:
                self.report.error(c.sha, None, f"{tag}: prefix has fewer than "
                                  f"{MIN_PREFIX_WORDS} words: {p.prefix!r}")
                continue
            pre = collapse(p.prefix)
            hits = [t for t in c.ticks if collapse(t.text).startswith(pre)]
            if not hits:
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

    def check_proof(self, c: Commit, p: ProvesLine, t: Tick) -> list[Definition]:
        defs, name = resolve(p.proof, self.tree)
        if not defs:
            self.report.error(c.sha, t.line, f"proof not found at the head: {p.proof!r}")
            return defs
        if p.existing is not None:
            self.report.existing.append(f"{c.sha[:7]} L{t.line}: {p.proof} "
                                        f"(existing: {p.existing})")
        elif not (_named_in(name, t.text) or any(touched(d, self.changed()) for d in defs)):
            self.report.error(c.sha, t.line, f"proof {p.proof!r} is not changed by the pull "
                              "request and not named in the box; mark it `(existing: <reason>)`")
        return defs

    def run(self) -> Report:
        for c in self.commits:
            self.report.ticks += len(c.ticks)
            self.check_commit(c)
        return self.report


def check(base: str, head: str, *, repo: Path = gatelib.ROOT) -> Report:
    return Checker(base, head, repo).run()


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
    args = ap.parse_args(argv)
    base = args.base
    if base is None:
        if gatelib.git(ROOT, "rev-parse", "--verify", "-q", "origin/main",
                       check=False).strip() == "":
            print("check_ticks: skipped (no origin/main)")
            return 0
        base = "origin/main"
    try:
        r = check(base, args.head, repo=ROOT)
    except gatelib.GateError as e:
        print(f"check_ticks: {e}", file=sys.stderr)
        return 1
    print_report(r)
    if r.errors:
        return 1
    print(f"check_ticks: ok ({r.ticks} ticks in {base}..{args.head})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
