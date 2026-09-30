#!/usr/bin/env python3
"""Gate inputs (ROADMAP §10.9): a pull request does not quietly change the gates
that judge it.

`tests/gates/inputs.toml` lists every file a gate reads to reach its verdict and
holds the `vibeos-core` coverage floor. Modes:

- bare: the static rules, then the diff rules from the merge base of
  `origin/main` and `HEAD` to `HEAD` when `origin/main` exists (otherwise the
  static rules only, saying so). `make check` runs it this way.
- `--base REV [--head REV]`: the static rules and the diff rules against REV's
  merge base with the head (default `HEAD`). The `check` job runs it on every
  pull request.
- `--floor`: print the floor, which the `check` job's llvm-cov step reads.
- `--summary (--tag TAG | --since REV) [--head REV]`: markdown for release.yml's
  `build` job summary: every input changed since the `v*` tag before TAG (or
  the root of history) or since REV, the floor at both ends, and the
  `Gate-change:` lines of the commits that changed each.

Static rules: `inputs.toml` follows its schema; each `path` exists, each
`recipe` is a Makefile rule and each `table` a table of its file; every file the
ROADMAP §10.9 box enumerates (`REQUIRED`) is listed; the floor is an integer 0
to 100; and no workflow holds a `--fail-under-lines <digits>` literal.

Diff rules, over the merge base to the head, on the union of both sides'
inputs (the head's alone when the base has no `inputs.toml`):

- a lowered or removed floor fails; no trailer allows it;
- a list entry (skip or expected-failure) at the head that is not identical to
  one at the base needs `Gate-change: <list> <entry> <class>: <reason>`, the
  class one of the list's classes and, when the entry sets a condition field,
  one it sets; a removed entry needs nothing; other changed content needs an
  input trailer;
- a `#### Fnnn` heading or `**Severity:**` line of the review changed, added or
  removed outside its `## Errata` section fails; any other change of the
  review needs an input trailer;
- `inputs.toml` needs an input trailer unless the head keeps every base entry,
  keeps or raises the floor, and changes nothing else;
- a changed or removed recipe or table, and a modified or deleted plain input,
  needs `Gate-change: <path>: <rule or gate line> -- <why>`; one added needs
  nothing.

A trailer counts from any non-merge commit of the pull request
(`gatelib.pr_commits`). A `Gate-change:` line that parses as neither form is a
warning and satisfies nothing.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402

INPUTS = "tests/gates/inputs.toml"
CI_YML = ".github/workflows/ci.yml"
WORKFLOWS = ".github/workflows/"
MAKEFILE = "Makefile"
TAG = "Gate-change"
FLOOR_LITERAL = re.compile(r"--fail-under-lines[ =]+(\d+)")
LIST_KINDS = ("skip", "expected-failure")
ENTRY_FIELDS = frozenset({"path", "glob", "why", "pair", "recipe", "table", "list", "review"})
LIST_FIELDS = frozenset({"kind", "table", "entry", "classes"})
REVIEW_LINE = re.compile(r"^(#### F\d{3}\b|\*\*Severity:\*\*)")
ERRATA = re.compile(r"^## Errata\s*$")

# The box's enumeration: globs of files that must be listed when they exist,
# and the recipes and tables (file, name) that must be listed.
REQUIRED_GLOBS: tuple[str, ...] = (
    "scripts/check_*.py",
    "scripts/gatelib.py",
    "scripts/ci_history.py",
    "scripts/gate.py",
    "scripts/release_check.py",
    "scripts/doc_refs.py",
    "tests/harness/test_gatelib.py",
    "tests/gates/*.toml",
    "tests/harness/skips.toml",
    "tests/contract/markers.toml",
    "deny.toml",
    ".github/workflows/*",
    "docs/reviews/KERNEL_REVIEW.md",
    "clippy.toml",
)
REQUIRED_PARTS: tuple[tuple[str, str, str], ...] = (
    ("recipe", MAKEFILE, "check"),
    ("recipe", MAKEFILE, "gate"),
    ("table", "Cargo.toml", "workspace.lints"),
    ("table", "pyproject.toml", "tool.ruff"),
    ("table", "pyproject.toml", "tool.mypy"),
)


class InputsError(Exception):
    """`inputs.toml` does not follow its schema."""


# --- trees -----------------------------------------------------------------


class Tree(Protocol):
    def read(self, path: str) -> bytes | None: ...

    def files(self) -> list[str]: ...


class GitTree:
    """The tree of commit `rev` in the repository at `root`."""

    def __init__(self, root: Path, rev: str) -> None:
        self.root = root
        self.rev = rev
        self._files: list[str] | None = None

    def read(self, path: str) -> bytes | None:
        r = subprocess.run(
            ["git", "-C", str(self.root), "cat-file", "blob", f"{self.rev}:{path}"],
            capture_output=True,
            check=False,
        )
        return r.stdout if r.returncode == 0 else None

    def files(self) -> list[str]:
        if self._files is None:
            out = gatelib.git(self.root, "ls-tree", "-r", "--name-only", self.rev)
            self._files = [p for p in out.split("\n") if p]
        return self._files


class WorkTree:
    """The files of the working tree: git's tracked and untracked, ignoring the
    ignored ones."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self._files: list[str] | None = None

    def read(self, path: str) -> bytes | None:
        p = self.root / path
        return p.read_bytes() if p.is_file() else None

    def files(self) -> list[str]:
        if self._files is None:
            out = gatelib.git(self.root, "ls-files", "--cached", "--others",
                              "--exclude-standard")
            self._files = sorted(p for p in set(out.split("\n")) if p
                                 and (self.root / p).is_file())
        return self._files


def text_of(tree: Tree, path: str) -> str | None:
    b = tree.read(path)
    return None if b is None else b.decode("utf-8", errors="replace")


# --- inputs.toml -----------------------------------------------------------


@dataclass(frozen=True)
class ListSpec:
    kind: str
    table: str
    entry: str
    classes: tuple[str, ...]


@dataclass(frozen=True)
class Entry:
    path: str | None
    glob: str | None
    why: str
    pair: str | None = None
    recipe: str | None = None
    table: str | None = None
    list: ListSpec | None = None
    review: bool = False
    raw: str = ""  # the entry as a canonical string, for the subset rule


@dataclass(frozen=True)
class Inputs:
    crate: str
    floor: int
    entries: tuple[Entry, ...]
    other: str  # everything but `[[input]]` and the floor, canonical


@dataclass(frozen=True)
class Item:
    """One input: a file, or a recipe or table of one."""

    path: str
    kind: str  # "plain", "recipe", "table", "list", "review", or "inputs"
    part: str | None = None  # the recipe or dotted table
    list: ListSpec | None = None

    @property
    def name(self) -> str:
        if self.kind == "recipe":
            return f"{self.path} `{self.part}` recipe"
        if self.kind == "table":
            return f"{self.path} [{self.part}]"
        return self.path


def _canon(v: object) -> str:
    if isinstance(v, dict):
        return "{" + ",".join(f"{k}={_canon(v[k])}" for k in sorted(v)) + "}"
    if isinstance(v, list):
        return "[" + ",".join(_canon(x) for x in v) + "]"
    return repr(v)


def _str(d: dict[str, Any], key: str, where: str) -> str:
    v = d.get(key)
    if not isinstance(v, str) or not v:
        raise InputsError(f"{where}: `{key}` is not a non-empty string")
    return v


def load_inputs(text: str) -> Inputs:
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise InputsError(f"{INPUTS}: {e}") from e
    extra = sorted(set(data) - {"coverage", "input"})
    if extra:
        raise InputsError(f"{INPUTS}: unknown table {', '.join(extra)}")
    cov = data.get("coverage")
    if not isinstance(cov, dict) or set(cov) != {"crate", "floor"}:
        raise InputsError(f"{INPUTS}: [coverage] holds exactly `crate` and `floor`")
    crate = _str(cov, "crate", f"{INPUTS} [coverage]")
    floor = cov["floor"]
    if not isinstance(floor, int) or isinstance(floor, bool) or not 0 <= floor <= 100:
        raise InputsError(f"{INPUTS}: [coverage].floor is not an integer 0 to 100")
    raw = data.get("input", [])
    if not isinstance(raw, list):
        raise InputsError(f"{INPUTS}: `input` is not an array of tables")
    entries: list[Entry] = []
    for i, r in enumerate(raw, start=1):
        where = f"{INPUTS} [[input]] {i}"
        if not isinstance(r, dict):
            raise InputsError(f"{where} is not a table")
        unknown = sorted(set(r) - ENTRY_FIELDS)
        if unknown:
            raise InputsError(f"{where}: unknown field {', '.join(unknown)}")
        if ("path" in r) == ("glob" in r):
            raise InputsError(f"{where}: exactly one of `path` and `glob`")
        path = _str(r, "path", where) if "path" in r else None
        glob = _str(r, "glob", where) if "glob" in r else None
        why = _str(r, "why", where)
        extras = [k for k in ("pair", "recipe", "table", "list", "review") if k in r]
        if len(extras) > 1:
            raise InputsError(f"{where}: at most one of {', '.join(extras)}")
        pair = recipe = table = None
        spec: ListSpec | None = None
        review = False
        if "pair" in r:
            pair = _str(r, "pair", where)
            if glob is None or "{stem}" not in pair:
                raise InputsError(f"{where}: `pair` needs a `glob` and holds `{{stem}}`")
        if "recipe" in r:
            recipe = _str(r, "recipe", where)
            if path != MAKEFILE:
                raise InputsError(f"{where}: `recipe` needs `path = \"{MAKEFILE}\"`")
        if "table" in r:
            table = _str(r, "table", where)
        if "list" in r:
            spec = _list_spec(r["list"], where)
        if "review" in r:
            if r["review"] is not True:
                raise InputsError(f"{where}: `review` is `true` or absent")
            review = True
        entries.append(Entry(path, glob, why, pair, recipe, table, spec, review, _canon(r)))
    other = _canon({"crate": crate})
    return Inputs(crate, floor, tuple(entries), other)


def _list_spec(v: object, where: str) -> ListSpec:
    if not isinstance(v, dict) or set(v) != LIST_FIELDS:
        raise InputsError(f"{where}: `list` holds exactly {', '.join(sorted(LIST_FIELDS))}")
    kind = _str(v, "kind", where)
    if kind not in LIST_KINDS:
        raise InputsError(f"{where}: list kind {kind!r} is not one of {', '.join(LIST_KINDS)}")
    classes = v["classes"]
    if not isinstance(classes, list) or not all(isinstance(c, str) and c for c in classes):
        raise InputsError(f"{where}: `list.classes` is not a list of strings")
    return ListSpec(kind, _str(v, "table", where), _str(v, "entry", where), tuple(classes))


def glob_regex(glob: str) -> re.Pattern[str]:
    """A glob whose `*` and `?` do not cross `/`."""
    out = []
    for ch in glob:
        if ch == "*":
            out.append("[^/]*")
        elif ch == "?":
            out.append("[^/]")
        else:
            out.append(re.escape(ch))
    return re.compile("".join(out))


def _stem(path: str) -> str:
    name = path.rsplit("/", 1)[-1]
    name = name[: -len(".py")] if name.endswith(".py") else name
    return name[len("check_"):] if name.startswith("check_") else name


def _item(path: str, e: Entry) -> Item:
    if path == INPUTS:
        return Item(path, "inputs")
    if e.review:
        return Item(path, "review")
    if e.list is not None:
        return Item(path, "list", list=e.list)
    if e.recipe is not None:
        return Item(path, "recipe", e.recipe)
    if e.table is not None:
        return Item(path, "table", e.table)
    return Item(path, "plain")


def _put(out: dict[tuple[str, str | None], Item], it: Item) -> None:
    """Add `it`; a list, review or `inputs.toml` item wins over a plain one for
    the same file, so a specific entry decides over a broad glob."""
    old = out.get((it.path, it.part))
    if old is None or (old.kind == "plain" and it.kind != "plain"):
        out[(it.path, it.part)] = it


def expand(inputs: Inputs, tree: Tree) -> list[Item]:
    """Every input `inputs` lists in `tree`: each `path`, each file a `glob`
    matches, and each existing `pair` of those."""
    files = tree.files()
    have = set(files)
    out: dict[tuple[str, str | None], Item] = {}
    for e in inputs.entries:
        if e.path is not None:
            _put(out, _item(e.path, e))
            continue
        assert e.glob is not None
        rx = glob_regex(e.glob)
        for f in files:
            if not rx.fullmatch(f):
                continue
            _put(out, _item(f, e))
            if e.pair is not None:
                p = e.pair.replace("{stem}", _stem(f))
                if p in have:
                    _put(out, Item(p, "plain"))
    return list(out.values())


def inputs_at(tree: Tree) -> Inputs | None:
    t = text_of(tree, INPUTS)
    return None if t is None else load_inputs(t)


def floor_at(tree: Tree) -> int | None:
    """`[coverage].floor`, or at a base older than `inputs.toml`, the
    `--fail-under-lines N` literal in ci.yml."""
    inputs = inputs_at(tree)
    if inputs is not None:
        return inputs.floor
    ci = text_of(tree, CI_YML)
    m = FLOOR_LITERAL.search(ci or "")
    return int(m.group(1)) if m else None


# --- parts of files --------------------------------------------------------


def recipe_text(makefile: str, target: str) -> str | None:
    """The rule line of `target:` and the tab-indented lines that follow it."""
    lines = makefile.splitlines()
    rule = re.compile(rf"^{re.escape(target)}\s*:(?![:=])")
    for n, raw in enumerate(lines):
        if rule.match(raw):
            body = [raw]
            for more in lines[n + 1:]:
                if not more.startswith("\t"):
                    break
                body.append(more)
            return "\n".join(body)
    return None


def table_value(text: str, dotted: str) -> object | None:
    try:
        data: object = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return None
    for k in dotted.split("."):
        if not isinstance(data, dict) or k not in data:
            return None
        data = data[k]
    return data


def part_of(tree: Tree, item: Item) -> str | None:
    text = text_of(tree, item.path)
    if text is None or item.part is None:
        return None
    if item.kind == "recipe":
        return recipe_text(text, item.part)
    v = table_value(text, item.part)
    return None if v is None else _canon(v)


def review_lines(text: str) -> list[str]:
    """The heading and severity lines outside `## Errata`."""
    out: list[str] = []
    for raw in text.splitlines():
        if ERRATA.match(raw):
            break
        if REVIEW_LINE.match(raw):
            out.append(raw)
    return out


# --- trailers --------------------------------------------------------------


@dataclass(frozen=True)
class Trailer:
    path: str
    entry: str | None  # set for an entry trailer
    cls: str | None
    text: str


def parse_trailer(line: str) -> Trailer | None:
    """One `Gate-change:` value, its prefix stripped: `<path>: <rule> -- <why>`
    (an input trailer) or `<list> <entry> <class>: <reason>` (an entry
    trailer). None when it is neither."""
    head, sep, body = line.partition(": ")
    if not sep:
        return None
    words = head.split()
    body = body.strip()
    if len(words) == 1:
        rule, dash, why = body.partition(" -- ")
        if not dash or not rule.strip() or not why.strip():
            return None
        return Trailer(words[0], None, None, line)
    if len(words) == 3 and body:
        return Trailer(words[0], words[1], words[2], line)
    return None


def _has_input_trailer(path: str, trailers: list[Trailer]) -> bool:
    return any(t.entry is None and t.path == path for t in trailers)


# --- diff rules ------------------------------------------------------------


def _list_rows(text: str | None, spec: ListSpec) -> tuple[list[dict[str, Any]], str] | None:
    """The rows of a list file and the canonical rest of it; None when unparsable."""
    if text is None:
        return [], _canon({})
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return None
    rows = data.pop(spec.table, [])
    if not isinstance(rows, list):
        return None
    return [r for r in rows if isinstance(r, dict)], _canon(data)


def _entry_ok(row: dict[str, Any], item: Item, trailers: list[Trailer]) -> bool:
    assert item.list is not None
    spec = item.list
    name = row.get(spec.entry)
    set_fields = [c for c in spec.classes if c in row]
    for t in trailers:
        if t.path != item.path or t.entry is None or t.entry != str(name):
            continue
        if t.cls not in spec.classes:
            continue
        if set_fields and t.cls not in set_fields:
            continue
        return True
    return False


def _list_errors(item: Item, base: Tree, head: Tree, trailers: list[Trailer]) -> list[str]:
    assert item.list is not None
    spec = item.list
    b_text, h_text = text_of(base, item.path), text_of(head, item.path)
    if b_text == h_text:
        return []
    errors: list[str] = []
    b = _list_rows(b_text, spec)
    if h_text is None:
        if not _has_input_trailer(item.path, trailers):
            errors.append(f"{item.path}: removed with no `{TAG}: {item.path}: <rule> -- <why>`")
        return errors
    h = _list_rows(h_text, spec)
    if b is None or h is None:
        if not _has_input_trailer(item.path, trailers):
            errors.append(f"{item.path}: not parsable as a {spec.kind} list and changed "
                          f"with no input trailer")
        return errors
    b_rows, b_rest = b
    h_rows, h_rest = h
    b_canon = {_canon(r) for r in b_rows}
    for r in h_rows:
        if _canon(r) in b_canon:
            continue
        if not _entry_ok(r, item, trailers):
            name = r.get(spec.entry, "?")
            set_fields = [c for c in spec.classes if c in r]
            cls = "|".join(set_fields or spec.classes)
            errors.append(f"{item.path}: {spec.kind} entry {name!r} added or changed with no "
                          f"`{TAG}: {item.path} {name} <{cls}>: <reason>`")
    if b_rest != h_rest and not _has_input_trailer(item.path, trailers):
        errors.append(f"{item.path}: content besides its entries changed with no "
                      f"`{TAG}: {item.path}: <rule> -- <why>`")
    return errors


def _inputs_errors(base: Tree, head: Tree, trailers: list[Trailer]) -> list[str]:
    b_text, h_text = text_of(base, INPUTS), text_of(head, INPUTS)
    if b_text == h_text or _has_input_trailer(INPUTS, trailers):
        return []
    if b_text is None:
        return []
    need = f"with no `{TAG}: {INPUTS}: <rule> -- <why>`"
    if h_text is None:
        return [f"{INPUTS}: removed {need}"]
    try:
        b, h = load_inputs(b_text), load_inputs(h_text)
    except InputsError as e:
        return [f"{e} (changed {need})"]
    h_raw = {e.raw for e in h.entries}
    missing = [e for e in b.entries if e.raw not in h_raw]
    errors = [f"{INPUTS}: entry {e.path or e.glob!r} removed or changed {need}" for e in missing]
    if b.other != h.other:
        errors.append(f"{INPUTS}: [coverage].crate changed {need}")
    return errors


def diff_errors(
    base: Tree, head: Tree, changed: list[tuple[str, str]], trailers: list[str]
) -> list[str]:
    """The diff rules from `base` to `head`. `changed` is (status, path) from
    `git diff --no-renames --name-status`; `trailers` are the `Gate-change:`
    values of the pull request's commits."""
    parsed = [t for t in (parse_trailer(x) for x in trailers) if t is not None]
    errors: list[str] = []
    b_floor, h_floor = floor_at(base), floor_at(head)
    if b_floor is not None and (h_floor is None or h_floor < b_floor):
        errors.append(f"coverage floor lowered from {b_floor} to {h_floor} "
                      f"(no trailer allows it)")
    b_inputs, h_inputs = inputs_at(base), inputs_at(head)
    items: dict[tuple[str, str | None], Item] = {}
    for inputs, tree in ((h_inputs, head), (b_inputs, base)):
        if inputs is None:
            continue
        for it in expand(inputs, tree):
            _put(items, it)
    status = {p: s for s, p in changed}
    for key in sorted(items, key=lambda k: (k[0], k[1] or "")):
        it = items[key]
        if it.kind == "inputs":
            errors += _inputs_errors(base, head, parsed)
        elif it.kind == "list":
            errors += _list_errors(it, base, head, parsed)
        elif it.kind == "review":
            errors += _review_errors(it, base, head, parsed)
        elif it.kind in ("recipe", "table"):
            b, h = part_of(base, it), part_of(head, it)
            if b is not None and b != h and not _has_input_trailer(it.path, parsed):
                what = "removed" if h is None else "changed"
                errors.append(f"{it.name}: {what} with no `{TAG}: {it.path}: <rule> -- <why>`")
        else:
            s = status.get(it.path, "")
            if s[:1] in ("M", "D", "T") and not _has_input_trailer(it.path, parsed):
                what = "deleted" if s.startswith("D") else "modified"
                errors.append(f"{it.path}: {what} with no `{TAG}: {it.path}: <rule> -- <why>`")
    return errors


def _review_errors(item: Item, base: Tree, head: Tree, trailers: list[Trailer]) -> list[str]:
    b_text, h_text = text_of(base, item.path), text_of(head, item.path)
    if b_text is None or b_text == h_text:
        return []
    errors: list[str] = []
    if h_text is None or review_lines(b_text) != review_lines(h_text):
        errors.append(f"{item.path}: a `#### Fnnn` heading or `**Severity:**` line changed "
                      f"outside `## Errata`; a correction is a dated erratum (no trailer "
                      f"allows it)")
    if not _has_input_trailer(item.path, trailers):
        errors.append(f"{item.path}: changed with no `{TAG}: {item.path}: <rule> -- <why>`")
    return errors


# --- static rules ----------------------------------------------------------


def static_errors(root: Path, tree: Tree | None = None) -> list[str]:
    tree = tree if tree is not None else WorkTree(root)
    text = text_of(tree, INPUTS)
    if text is None:
        return [f"{INPUTS}: missing"]
    try:
        inputs = load_inputs(text)
    except InputsError as e:
        return [str(e)]
    errors: list[str] = []
    have = set(tree.files())
    for ent in inputs.entries:
        if ent.path is not None and ent.path not in have:
            errors.append(f"{INPUTS}: path {ent.path!r} does not exist")
            continue
        if ent.recipe is not None and ent.path is not None:
            if recipe_text(text_of(tree, ent.path) or "", ent.recipe) is None:
                errors.append(f"{INPUTS}: {ent.path} has no `{ent.recipe}` recipe")
        if ent.table is not None and ent.path is not None:
            if table_value(text_of(tree, ent.path) or "", ent.table) is None:
                errors.append(f"{INPUTS}: {ent.path} has no [{ent.table}] table")
        if ent.list is not None and ent.path is not None:
            if _list_rows(text_of(tree, ent.path), ent.list) is None:
                errors.append(f"{INPUTS}: {ent.path} is not a list with [[{ent.list.table}]] rows")
    items = expand(inputs, tree)
    listed = {(it.path, it.part) for it in items}
    for g in REQUIRED_GLOBS:
        rx = glob_regex(g)
        for f in sorted(have):
            if rx.fullmatch(f) and (f, None) not in listed:
                errors.append(f"{INPUTS}: {f} is a gate input and is not listed")
    for f in sorted(have):
        m = re.fullmatch(r"scripts/check_(\w+)\.py", f)
        test = f"tests/harness/test_{m.group(1)}.py" if m else None
        if test is not None and test in have and (test, None) not in listed:
            errors.append(f"{INPUTS}: {test} (the test of {f}) is not listed")
    for kind, path, part in REQUIRED_PARTS:
        if path in have and (path, part) not in listed:
            errors.append(f"{INPUTS}: the {kind} {path} {part!r} is not listed")
    for f in sorted(have):
        if not f.startswith(WORKFLOWS):
            continue
        for n, raw in enumerate((text_of(tree, f) or "").splitlines(), start=1):
            if FLOOR_LITERAL.search(raw):
                errors.append(f"{f}:{n}: a `--fail-under-lines` literal; read the floor with "
                              f"`check_gate_inputs.py --floor`")
    return errors



# --- release summary -------------------------------------------------------

EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"


def previous_tag(root: Path, tag: str) -> str | None:
    """The `v*` tag before `tag`'s commit, or None at the root of history."""
    r = gatelib.git(root, "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*",
                    f"{tag}^{{commit}}^", check=False).strip()
    return r or None


def changed_inputs(since: Tree, head: Tree, changed: list[tuple[str, str]]) -> list[Item]:
    """The inputs of either side that differ between them: a file changed, or a
    recipe or table whose text changed."""
    items: dict[tuple[str, str | None], Item] = {}
    for tree in (head, since):
        inputs = inputs_at(tree)
        if inputs is not None:
            for it in expand(inputs, tree):
                _put(items, it)
    paths = {p for _, p in changed}
    out: list[Item] = []
    for key in sorted(items, key=lambda k: (k[0], k[1] or "")):
        it = items[key]
        if it.kind in ("recipe", "table"):
            if part_of(since, it) != part_of(head, it):
                out.append(it)
        elif it.path in paths:
            out.append(it)
    return out


def summary(root: Path, since: str | None, head: str, label: str | None = None) -> str:
    """Markdown: every gate input changed from `since` (None: the root of
    history) to `head`, the floor at both ends, and under each input the
    `Gate-change:` lines of each commit that changed it: those naming its path,
    or all the commit's lines when none does."""
    base = since if since is not None else EMPTY_TREE
    changed: list[tuple[str, str]] = []
    for raw in gatelib.git(root, "diff", "--no-renames", "--name-status", base, head).split("\n"):
        if raw:
            s, _, p = raw.partition("\t")
            changed.append((s, p))
    since_tree: Tree = GitTree(root, since) if since is not None else _EmptyTree()
    head_tree = GitTree(root, head)
    items = changed_inputs(since_tree, head_tree, changed)
    head_sha = gatelib.git(root, "rev-parse", "--short", f"{head}^{{commit}}").strip()
    start = label or (since if since is not None else "the root of history")
    lines = [f"## Gate inputs changed since {start}", ""]
    lines.append(f"Coverage floor: {floor_at(since_tree)} at {start}, {floor_at(head_tree)} "
                 f"at {head_sha}.")
    lines.append("")
    if not items:
        lines.append("No gate input changed.")
    rng = f"{since}..{head}" if since is not None else head
    for it in items:
        lines.append(f"- {it.name}")
        shas = gatelib.git(root, "log", "--no-merges", "--format=%H", rng, "--",
                           it.path).split()
        for sha in reversed(shas):
            found = gatelib.message_lines(sha, TAG, root)
            if not found:
                lines.append(f"  - `{sha[:10]}` no `{TAG}:` line")
                continue
            naming = [t for t in found if t.split(":", 1)[0].split()[:1] == [it.path]]
            for t in naming or found:
                lines.append(f"  - `{sha[:10]}` {TAG}: {t}")
    return "\n".join(lines) + "\n"


class _EmptyTree:
    def read(self, path: str) -> bytes | None:
        return None

    def files(self) -> list[str]:
        return []


# --- main ------------------------------------------------------------------


def _ref_exists(root: Path, ref: str) -> bool:
    return bool(gatelib.git(root, "rev-parse", "--verify", "-q", f"{ref}^{{commit}}",
                            check=False).strip())


def run_diff(root: Path, base: str, head: str) -> tuple[list[str], list[str]]:
    """Errors and warnings of the diff rules from `base`'s merge base with `head`."""
    mb = gatelib.git(root, "merge-base", base, head).strip()
    changed: list[tuple[str, str]] = []
    for raw in gatelib.git(root, "diff", "--no-renames", "--name-status", mb, head).split("\n"):
        if raw:
            s, _, p = raw.partition("\t")
            changed.append((s, p))
    trailers: list[str] = []
    for sha in gatelib.pr_commits(mb, head, root):
        trailers += gatelib.message_lines(sha, TAG, root)
    warnings = [f"unparsed `{TAG}: {t}` satisfies nothing" for t in trailers
                if parse_trailer(t) is None]
    return diff_errors(GitTree(root, mb), GitTree(root, head), changed, trailers), warnings


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", help="diff against this revision's merge base with the head")
    ap.add_argument("--head", default="HEAD", help="the pull request's head (default HEAD)")
    ap.add_argument("--floor", action="store_true", help="print the coverage floor")
    ap.add_argument("--summary", action="store_true",
                    help="markdown: the inputs changed since the previous release (or --since)")
    ap.add_argument("--tag", help="--summary: the release tag; since the `v*` tag before it")
    ap.add_argument("--since", help="--summary: since this revision")
    args = ap.parse_args(argv)
    root = ROOT
    if args.summary:
        if (args.tag is None) == (args.since is None):
            ap.error("--summary takes exactly one of --tag and --since")
        try:
            if args.tag is not None:
                head = args.head if args.head != "HEAD" else f"{args.tag}^{{commit}}"
                since = previous_tag(root, args.tag)
                label = since
            else:
                head, since, label = args.head, args.since, args.since
            print(summary(root, since, head, label), end="")
        except (gatelib.GateError, InputsError) as e:
            print(f"check_gate_inputs: {e}", file=sys.stderr)
            return 1
        return 0
    if args.floor:
        try:
            floor = floor_at(WorkTree(root))
        except InputsError as e:
            print(e, file=sys.stderr)
            return 1
        if floor is None:
            print(f"check_gate_inputs: no floor in {INPUTS}", file=sys.stderr)
            return 1
        print(floor)
        return 0
    errors = static_errors(root)
    warnings: list[str] = []
    base = args.base
    note = ""
    if base is None:
        if _ref_exists(root, "origin/main"):
            base = "origin/main"
        else:
            note = " (static rules only: no origin/main)"
    if base is not None and not errors:
        try:
            d_errors, warnings = run_diff(root, base, args.head)
        except (gatelib.GateError, InputsError) as e:
            d_errors = [str(e)]
        errors += d_errors
        note = f" (against {base})"
    for w in warnings:
        print(f"check_gate_inputs: warning: {w}", file=sys.stderr)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"check_gate_inputs: ok{note}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
