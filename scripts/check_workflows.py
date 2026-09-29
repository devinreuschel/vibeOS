#!/usr/bin/env python3
"""GitHub workflow rules (ROADMAP §10.1, DESIGN §8.6).

One function per rule, each `Tree -> list[Problem]`, all listed in `RULES`.
The workflows are read by a YAML subset reader (`parse`), stdlib only: block
mappings and sequences, `|`/`>` block scalars, quoted and plain scalars,
one-line flow collections, and comments. It raises `Unsupported` on anchors,
aliases, tags, `---`, merge keys, multi-line flow collections and multi-line
plain scalars, so nothing is misread silently.

Output: `path:line: [rule] message` per problem, or `check_workflows: ok`.
"""

from __future__ import annotations

import re
import sys
import tomllib
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS_DIR = ".github/workflows"


class Unsupported(Exception):
    def __init__(self, path: str, line: int, what: str) -> None:
        super().__init__(f"{path}:{line}: unsupported YAML: {what}")
        self.path = path
        self.line = line
        self.what = what


@dataclass
class Node:
    """A value, with the key it sits under when it is a mapping entry.

    A scalar has `value` set; a mapping has `children` (entries, each a Node
    with `key`); a sequence has `items`. `kind` says which, or `null`.
    `comment` is the trailing comment on the node's line. `line` is 1-based.
    """

    line: int
    key: str | None = None
    value: str | None = None
    comment: str | None = None
    kind: str = "null"
    children: list[Node] = field(default_factory=list)
    items: list[Node] = field(default_factory=list)
    # First line of a block scalar's content (its own line otherwise).
    value_line: int = 0

    def get(self, key: str) -> Node | None:
        for c in self.children:
            if c.key == key:
                return c
        return None

    def scalars(self) -> list[str]:
        """The scalar itself, or a sequence's scalar items."""
        if self.kind == "scalar" and self.value is not None:
            return [self.value]
        if self.kind == "seq":
            return [i.value for i in self.items if i.kind == "scalar" and i.value is not None]
        return []

    def walk(self) -> Iterator[Node]:
        yield self
        for c in self.children:
            yield from c.walk()
        for i in self.items:
            yield from i.walk()


@dataclass
class _Line:
    no: int
    indent: int
    text: str  # without indentation and trailing comment
    comment: str | None


def _strip_comment(s: str) -> tuple[str, str | None]:
    """Split `s` at a comment `#` (at the start or after whitespace, outside quotes)."""
    quote = ""
    prev = " "
    for i, ch in enumerate(s):
        if quote:
            if ch == quote:
                if quote == "'" and s[i + 1 : i + 2] == "'":
                    continue
                if quote == '"' and prev == "\\":
                    prev = ch
                    continue
                quote = ""
        elif ch in "\"'" and prev in " \t[{,:":
            quote = ch
        elif ch == "#" and prev in " \t":
            return s[:i].rstrip(), s[i + 1 :].strip()
        prev = ch
    return s.rstrip(), None


def _split_key(text: str) -> tuple[str, str] | None:
    """`key: rest` → (key, rest); None when `text` is not a mapping entry."""
    if not text or text[0] in "[{":
        return None
    if text[0] in "\"'":
        q = text[0]
        end = text.find(q, 1)
        if end < 0:
            return None
        after = text[end + 1 :]
        if after == ":" or after.startswith(": "):
            return text[1:end], after[1:].strip()
        return None
    m = re.match(r"([^:#]*?[^\s:]):(?:\s+(.*)|)$", text)
    if m is None or "${{" in m.group(1):
        return None
    return m.group(1), (m.group(2) or "").strip()


class _Parser:
    def __init__(self, text: str, path: str) -> None:
        self.path = path
        self.raw = text.splitlines()
        self.lines: list[_Line] = []
        self.pos = 0
        for no, raw in enumerate(self.raw, 1):
            stripped = raw.lstrip(" ")
            if "\t" in raw[: len(raw) - len(stripped)]:
                raise Unsupported(path, no, "tab indentation")
            if stripped.rstrip() in ("---", "..."):
                raise Unsupported(path, no, "document marker")
            body, comment = _strip_comment(stripped)
            self.lines.append(_Line(no, len(raw) - len(stripped), body, comment))

    def _skip_blank(self) -> None:
        while self.pos < len(self.lines) and self.lines[self.pos].text == "":
            self.pos += 1

    def _peek(self) -> _Line | None:
        self._skip_blank()
        return self.lines[self.pos] if self.pos < len(self.lines) else None

    def fail(self, line: int, what: str) -> Unsupported:
        return Unsupported(self.path, line, what)

    def parse(self) -> Node:
        first = self._peek()
        if first is None:
            return Node(line=1, kind="map")
        node = self._block(first.indent)
        rest = self._peek()
        if rest is not None:
            raise self.fail(rest.no, "text outside the document's structure")
        return node

    def _block(self, indent: int) -> Node:
        ln = self._peek()
        assert ln is not None
        if ln.text == "-" or ln.text.startswith("- "):
            return self._seq(indent)
        return self._map(indent)

    def _map(self, indent: int) -> Node:
        first = self._peek()
        assert first is not None
        node = Node(line=first.no, kind="map")
        while True:
            ln = self._peek()
            if ln is None or ln.indent < indent:
                break
            if ln.indent > indent:
                raise self.fail(ln.no, "unexpected indentation")
            if ln.text == "-" or ln.text.startswith("- "):
                break
            kv = _split_key(ln.text)
            if kv is None:
                raise self.fail(ln.no, f"expected `key: value`, got {ln.text!r}")
            key, rest = kv
            if key == "<<":
                raise self.fail(ln.no, "merge key")
            self.pos += 1
            entry = self._value(rest, ln, indent)
            entry.key = key
            entry.comment = ln.comment
            if node.get(key) is not None:
                raise self.fail(ln.no, f"duplicate key {key!r}")
            node.children.append(entry)
        return node

    def _seq(self, indent: int) -> Node:
        first = self._peek()
        assert first is not None
        node = Node(line=first.no, kind="seq")
        while True:
            ln = self._peek()
            if ln is None or ln.indent < indent:
                break
            if ln.indent > indent:
                raise self.fail(ln.no, "unexpected indentation")
            if not (ln.text == "-" or ln.text.startswith("- ")):
                break
            rest = ln.text[1:].lstrip()
            if rest and _split_key(rest) is not None and rest[0] not in "\"'[{":
                # `- key: value` opens a mapping whose keys sit at the column of `key`.
                offset = len(ln.text) - len(rest)
                self.lines[self.pos] = _Line(ln.no, indent + offset, rest, ln.comment)
                item = self._map(indent + offset)
            else:
                self.pos += 1
                item = self._value(rest, ln, indent)
                item.comment = ln.comment
            node.items.append(item)
        return node

    def _value(self, rest: str, ln: _Line, indent: int) -> Node:
        """The value after `key:` or `-` on line `ln`, whose own indent is `indent`."""
        if rest[:1] in ("&", "*", "!"):
            raise self.fail(ln.no, "anchor, alias or tag")
        if rest == "":
            nxt = self._peek()
            if nxt is not None and nxt.indent > indent:
                return self._block(nxt.indent)
            if (
                nxt is not None
                and nxt.indent == indent
                and (nxt.text == "-" or nxt.text.startswith("- "))
                and ln.text != "-"
                and not ln.text.startswith("- ")
            ):
                return self._seq(indent)
            return Node(line=ln.no, kind="null", value_line=ln.no)
        if re.fullmatch(r"[|>][-+]?", rest):
            return self._block_scalar(rest[0], ln, indent)
        if rest[0] in "[{":
            node = self._flow(rest, ln.no)
        else:
            node = Node(line=ln.no, kind="scalar", value=self._scalar(rest, ln.no))
        node.value_line = ln.no
        nxt = self._peek()
        if nxt is not None and nxt.indent > indent:
            raise self.fail(nxt.no, "multi-line plain scalar")
        return node

    def _block_scalar(self, style: str, ln: _Line, indent: int) -> Node:
        body: list[str] = []
        block_indent = -1
        first_no = 0
        i = self.pos
        while i < len(self.raw):
            raw = self.raw[i]
            if raw.strip() == "":
                body.append("")
                i += 1
                continue
            ind = len(raw) - len(raw.lstrip(" "))
            if ind <= indent:
                break
            if block_indent < 0:
                block_indent = ind
                first_no = i + 1
            if ind < block_indent:
                break
            body.append(raw[block_indent:])
            i += 1
        while body and body[-1] == "":
            body.pop()
        self.pos = i
        sep = "\n" if style == "|" else " "
        return Node(
            line=ln.no, kind="scalar", value=sep.join(body), value_line=first_no or ln.no
        )

    def _scalar(self, s: str, no: int) -> str:
        if s[0] == '"':
            if len(s) < 2 or not s.endswith('"'):
                raise self.fail(no, "unterminated double-quoted scalar")
            inner = s[1:-1]
            return re.sub(r'\\(["\\/nt])', lambda m: {"n": "\n", "t": "\t"}.get(m[1], m[1]), inner)
        if s[0] == "'":
            if len(s) < 2 or not s.endswith("'"):
                raise self.fail(no, "unterminated single-quoted scalar")
            return s[1:-1].replace("''", "'")
        if s[0] in "&*!":
            raise self.fail(no, "anchor, alias or tag")
        return s

    def _flow(self, s: str, no: int) -> Node:
        close = {"[": "]", "{": "}"}[s[0]]
        depth = 0
        quote = ""
        parts: list[str] = []
        cur = ""
        end = -1
        for i, ch in enumerate(s):
            if quote:
                cur += ch
                if ch == quote:
                    quote = ""
                continue
            if ch in "\"'":
                quote = ch
                cur += ch
                continue
            if ch in "[{":
                depth += 1
                if depth == 1:
                    continue
            elif ch in "]}":
                depth -= 1
                if depth == 0:
                    end = i
                    break
            elif ch == "," and depth == 1:
                parts.append(cur.strip())
                cur = ""
                continue
            cur += ch
        if end < 0:
            raise self.fail(no, "multi-line flow collection")
        if s[end] != close or s[end + 1 :].strip():
            raise self.fail(no, "malformed flow collection")
        if cur.strip():
            parts.append(cur.strip())
        if close == "]":
            items = []
            for p in parts:
                if p[:1] in "[{":
                    raise self.fail(no, "nested flow collection")
                items.append(Node(line=no, kind="scalar", value=self._scalar(p, no), value_line=no))
            return Node(line=no, kind="seq", items=items)
        children = []
        for p in parts:
            kv = _split_key(p)
            if kv is None:
                raise self.fail(no, f"flow mapping entry {p!r}")
            v = Node(line=no, kind="scalar", value=self._scalar(kv[1], no), value_line=no)
            if kv[1] == "":
                v = Node(line=no, kind="null")
            v.key = kv[0]
            children.append(v)
        return Node(line=no, kind="map", children=children)


def parse(text: str, path: str) -> Node:
    return _Parser(text, path).parse()


@dataclass(frozen=True)
class Problem:
    path: str
    line: int
    rule: str
    message: str

    def __str__(self) -> str:
        return f"{self.path}:{self.line}: [{self.rule}] {self.message}"


@dataclass
class Tree:
    """Everything the rules read. `workflows` maps `.github/workflows/<f>` to its root."""

    workflows: dict[str, Node]
    makefile: str = ""
    testing_md: str = ""
    upstream_md: str | None = None
    # (gate file path, workflow it names) for each `[[line.entry]] job = {workflow, ...}`.
    gate_workflows: list[tuple[str, str]] = field(default_factory=list)
    root: Path = ROOT


def _gate_workflows(root: Path) -> list[tuple[str, str]]:
    out: list[tuple[str, str]] = []
    for p in sorted((root / "tests/gates").glob("*.toml")):
        data = tomllib.loads(p.read_text(encoding="utf-8"))
        lines = data.get("line", [])
        if not isinstance(lines, list):
            continue
        for line in lines:
            entries = line.get("entry", []) if isinstance(line, dict) else []
            for entry in entries if isinstance(entries, list) else []:
                job = entry.get("job") if isinstance(entry, dict) else None
                if isinstance(job, dict) and isinstance(job.get("workflow"), str):
                    out.append((str(p.relative_to(root)), job["workflow"]))
    return out


def load_tree(root: Path = ROOT) -> Tree:
    workflows: dict[str, Node] = {}
    for p in sorted((root / WORKFLOWS_DIR).glob("*.y*ml")):
        rel = str(p.relative_to(root))
        workflows[rel] = parse(p.read_text(encoding="utf-8"), rel)
    upstream = root / "docs/UPSTREAM.md"
    return Tree(
        workflows=workflows,
        makefile=(root / "Makefile").read_text(encoding="utf-8"),
        testing_md=(root / "docs/TESTING.md").read_text(encoding="utf-8"),
        upstream_md=upstream.read_text(encoding="utf-8") if upstream.exists() else None,
        gate_workflows=_gate_workflows(root),
        root=root,
    )


def _jobs(wf: Node) -> list[Node]:
    jobs = wf.get("jobs")
    return jobs.children if jobs is not None else []


def rule_no_expr_in_run(tree: Tree) -> list[Problem]:
    """F144: `${{` inside a `run:` script is template injection; pass it through `env:`."""
    out = []
    for path, wf in tree.workflows.items():
        for n in wf.walk():
            if n.key != "run" or n.kind != "scalar" or n.value is None:
                continue
            for off, text in enumerate(n.value.split("\n")):
                if "${{" in text:
                    line = n.value_line + off if n.value_line != n.line else n.line
                    out.append(
                        Problem(path, line, "no_expr_in_run", "`${{` inside `run:`; use `env:`")
                    )
    return out


def _grants(perm: Node, path: str, where: str) -> list[Problem]:
    if perm.kind == "scalar":
        return [Problem(path, perm.line, "permissions", f"{where} `permissions: {perm.value}`")]
    out = []
    for g in perm.children:
        if g.value == "none" or (g.key == "contents" and g.value == "read"):
            continue
        if not g.comment:
            out.append(
                Problem(
                    path,
                    g.line,
                    "permissions",
                    f"{where} grant `{g.key}: {g.value}` names no need in a comment beside it",
                )
            )
    return out


def rule_permissions(tree: Tree) -> list[Problem]:
    """Least privilege: a top-level mapping; any grant past `contents: read` says why."""
    out = []
    for path, wf in tree.workflows.items():
        top = wf.get("permissions")
        if top is None or top.kind not in ("map", "scalar"):
            out.append(Problem(path, 1, "permissions", "no top-level `permissions:` mapping"))
        else:
            out += _grants(top, path, "workflow")
        for job in _jobs(wf):
            perm = job.get("permissions")
            if perm is not None:
                out += _grants(perm, path, f"job `{job.key}`")
    return out


PIN_RE = re.compile(r"[^@\s]+@[0-9a-f]{40}")


def rule_action_pins(tree: Tree) -> list[Problem]:
    """Exit-gate line 1147 (#93 §9 D-11): remote actions pinned to a commit and its version."""
    out = []
    for path, wf in tree.workflows.items():
        for n in wf.walk():
            if n.key != "uses" or n.value is None:
                continue
            v = n.value
            if v.startswith("./"):
                continue
            if v.startswith("docker://"):
                if "@sha256:" not in v:
                    out.append(Problem(path, n.line, "action_pins", f"{v}: no @sha256: digest"))
                continue
            if not PIN_RE.fullmatch(v) or not n.comment:
                out.append(
                    Problem(path, n.line, "action_pins", f"{v}: not `@<40 hex>  # <version>`")
                )
    return out


RULES: list[Callable[[Tree], list[Problem]]] = [
    rule_no_expr_in_run,
    rule_permissions,
    rule_action_pins,
]


def check(tree: Tree) -> list[Problem]:
    out: list[Problem] = []
    for rule in RULES:
        out += rule(tree)
    return out


def main(argv: list[str] | None = None) -> int:
    del argv
    try:
        problems = check(load_tree(ROOT))
    except Unsupported as e:
        print(e, file=sys.stderr)
        return 1
    for p in problems:
        print(p, file=sys.stderr)
    if problems:
        return 1
    print("check_workflows: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
