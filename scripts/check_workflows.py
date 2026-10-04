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

import fnmatch
import re
import sys
import tomllib
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS_DIR = ".github/workflows"
CI = f"{WORKFLOWS_DIR}/ci.yml"


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
            entry.line = ln.no
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


RUNNERS = ("ubuntu-26.04", "ubuntu-26.04-arm")
MATRIX_REF_RE = re.compile(r"\$\{\{\s*matrix\.([A-Za-z0-9_-]+)\s*\}\}")


def _matrix_values(job: Node, name: str) -> list[str]:
    """Every value `matrix.<name>` takes: the axis list and each `include` entry's."""
    strategy = job.get("strategy")
    matrix = strategy.get("matrix") if strategy is not None else None
    if matrix is None:
        return []
    out: list[str] = []
    axis = matrix.get(name)
    if axis is not None:
        out += axis.scalars()
    include = matrix.get("include")
    for entry in include.items if include is not None else []:
        v = entry.get(name)
        if v is not None:
            out += v.scalars()
    return out


def rule_runs_on(tree: Tree) -> list[Problem]:
    """L1203: Linux jobs run on GitHub's free `ubuntu-26.04` image (`-arm` for arm64)."""
    out = []
    for path, wf in tree.workflows.items():
        for job in _jobs(wf):
            runs_on = job.get("runs-on")
            if runs_on is None:
                continue
            labels: list[str] = []
            for label in runs_on.scalars():
                m = MATRIX_REF_RE.fullmatch(label.strip())
                if m is None:
                    labels.append(label)
                    continue
                values = _matrix_values(job, m.group(1))
                if not values:
                    out.append(
                        Problem(path, runs_on.line, "runs_on", f"{label}: matrix has no values")
                    )
                labels += values
            if runs_on.kind not in ("scalar", "seq"):
                out.append(Problem(path, runs_on.line, "runs_on", "`runs-on:` is not a label"))
            for label in labels:
                if label not in RUNNERS and not label.startswith("macos-"):
                    out.append(
                        Problem(
                            path,
                            runs_on.line,
                            "runs_on",
                            f"job `{job.key}` runs on {label!r}; Linux jobs use ubuntu-26.04",
                        )
                    )
    return out


PIN_VERSION_RE = re.compile(r"\d+\.\d+\.\d+")


def _names(node: Node, needle: str) -> bool:
    return any(
        needle in (n.value or "") or needle in (n.key or "") for n in node.walk()
    )


def rule_qemu_pin(tree: Tree) -> list[Problem]:
    """L1203: a job that names `qemu-system` pins the QEMU the harness checks it against."""
    out = []
    for path, wf in tree.workflows.items():
        top_env = wf.get("env")
        top_pin = top_env.get("VIBEOS_QEMU_VERSION") if top_env is not None else None
        for job in _jobs(wf):
            if not _names(job, "qemu-system"):
                continue
            env = job.get("env")
            pin = env.get("VIBEOS_QEMU_VERSION") if env is not None else None
            pin = pin or top_pin
            if pin is None or not PIN_VERSION_RE.fullmatch(pin.value or ""):
                out.append(
                    Problem(
                        path,
                        job.line,
                        "qemu_pin",
                        f"job `{job.key}` names qemu-system but pins no "
                        "`VIBEOS_QEMU_VERSION: \"N.N.N\"`",
                    )
                )
    return out


FILTERS = ("tags", "tags-ignore", "branches-ignore", "paths", "paths-ignore")


def rule_ci_triggers(tree: Tree) -> list[Problem]:
    """L1205: ci.yml runs on push to `main`, every pull request and dispatch, grouped per PR."""
    wf = tree.workflows.get(CI)
    if wf is None:
        return [Problem(CI, 1, "ci_triggers", "ci.yml is missing")]
    out = []
    on = wf.get("on")
    if on is None or on.kind != "map":
        return [Problem(CI, on.line if on else 1, "ci_triggers", "`on:` is not a mapping")]
    push = on.get("push")
    if push is None:
        out.append(Problem(CI, on.line, "ci_triggers", "no `push` trigger"))
    else:
        branches = push.get("branches")
        names = branches.scalars() if branches is not None else []
        if branches is None or names != ["main"]:
            out.append(
                Problem(CI, push.line, "ci_triggers", f"push branches {names} are not [main]")
            )
    for event in ("push", "pull_request"):
        node = on.get(event)
        for key in FILTERS:
            f = node.get(key) if node is not None else None
            if f is not None:
                out.append(Problem(CI, f.line, "ci_triggers", f"`{event}` has a `{key}` filter"))
    for event in ("pull_request", "workflow_dispatch"):
        if on.get(event) is None:
            out.append(Problem(CI, on.line, "ci_triggers", f"no `{event}` trigger"))
    conc = wf.get("concurrency")
    group = conc.get("group") if conc is not None else None
    text = (group.value if group is not None else None) or ""
    m = re.search(r"'ci-pr-[^']*'\s*,\s*github\.event\.pull_request\.number", text)
    if m is None or "github.run_id" not in text:
        out.append(
            Problem(
                CI,
                (group or conc or wf).line,
                "ci_triggers",
                "concurrency group must be `ci-pr-<pull_request.number>` for a pull request "
                "and keyed by `github.run_id` otherwise",
            )
        )
    return out


def _groups(wf: Node) -> list[Node]:
    out = []
    for holder in [wf, *_jobs(wf)]:
        conc = holder.get("concurrency")
        if conc is None:
            continue
        group = conc.get("group") if conc.kind == "map" else conc
        if group is not None:
            out.append(group)
    return out


def rule_concurrency_group(tree: Tree) -> list[Problem]:
    """L1205: a group from the branch name lets unrelated runs (a fork's `main`) share it."""
    out = []
    for path, wf in tree.workflows.items():
        for group in _groups(wf):
            for ref in ("github.head_ref", "github.ref_name"):
                if ref in (group.value or ""):
                    out.append(
                        Problem(path, group.line, "concurrency_group", f"group built from {ref}")
                    )
    return out


def rule_gate_dispatch(tree: Tree) -> list[Problem]:
    """L1205: a workflow a gate entry names can be dispatched, so the gate can rerun it."""
    out = []
    by_name: dict[str, Node] = {}
    for path, wf in tree.workflows.items():
        name = Path(path).name
        by_name[name] = wf
        by_name[Path(path).stem] = wf
        title = wf.get("name")
        if title is not None and title.value:
            by_name.setdefault(title.value, wf)
    for gate, workflow in tree.gate_workflows:
        target = by_name.get(workflow)
        if target is None:
            out.append(Problem(gate, 1, "gate_dispatch", f"names unknown workflow {workflow!r}"))
            continue
        on = target.get("on")
        dispatch = on is not None and (
            on.get("workflow_dispatch") is not None or "workflow_dispatch" in on.scalars()
        )
        if not dispatch:
            out.append(
                Problem(
                    gate, 1, "gate_dispatch", f"workflow {workflow!r} has no workflow_dispatch"
                )
            )
    return out


@dataclass(frozen=True)
class TierEntry:
    arch: str
    tier: str
    targets: tuple[str, ...]
    line: int


def _tier_entries(job: Node) -> list[TierEntry]:
    strategy = job.get("strategy")
    matrix = strategy.get("matrix") if strategy is not None else None
    include = matrix.get("include") if matrix is not None else None
    out = []
    for e in include.items if include is not None else []:
        vals: dict[str, str | None] = {}
        for k in _MATRIX_KEYS:
            v = e.get(k)
            vals[k] = v.value if v is not None else None
        out.append(
            TierEntry(
                vals["arch"] or "",
                vals["tier"] or "",
                tuple((vals["targets"] or "").split()),
                e.line,
            )
        )
    return out


_MATRIX_KEYS = ("arch", "tier", "targets", "jobs")


def _make_prereqs(makefile: str, target: str) -> list[str]:
    """The prerequisites of `target:` in `makefile`, continuation lines joined."""
    text = makefile.replace("\\\n", " ")
    m = re.search(rf"^{re.escape(target)}:([^=\n]*)$", text, re.M)
    return m.group(1).split() if m else []


def _check_submakes(makefile: str) -> set[str]:
    """Targets the `check` recipe runs as `$(MAKE) <t>`: the `check` job covers them."""
    m = re.search(r"^check:.*\n((?:\t.*\n|\n)*)", makefile, re.M)
    return set(re.findall(r"\$\(MAKE\) (\S+)", m.group(1))) if m else set()


def _is_rule(makefile: str, target: str) -> bool:
    return re.search(rf"^{re.escape(target)}:(?!=)", makefile, re.M) is not None


def rule_tiers(tree: Tree) -> list[Problem]:
    """L1199: check, build and a tier matrix running every `make test` tier exactly once."""
    wf = tree.workflows.get(CI)
    if wf is None:
        return [Problem(CI, 1, "tiers", "ci.yml is missing")]
    out = []
    jobs = {j.key: j for j in _jobs(wf)}
    for name in ("check", "build", "tier"):
        if name not in jobs:
            out.append(Problem(CI, 1, "tiers", f"no `{name}` job"))
    tier = jobs.get("tier")
    if tier is None:
        return out
    needs = tier.get("needs")
    if needs is None or not {"check", "build"} <= set(needs.scalars()):
        out.append(Problem(CI, tier.line, "tiers", "`tier` needs must include check and build"))
    strategy = tier.get("strategy")
    ff = strategy.get("fail-fast") if strategy is not None else None
    if ff is None or ff.value != "false":
        out.append(Problem(CI, tier.line, "tiers", "`tier` must set `fail-fast: false`"))
    steps = tier.get("steps")
    runs = [st.get("run") for st in (steps.items if steps is not None else [])]
    if not any(r is not None and "VIBEOS_PREBUILT=1" in (r.value or "") for r in runs):
        out.append(Problem(CI, tier.line, "tiers", "no `tier` run step sets VIBEOS_PREBUILT=1"))
    matrix = strategy.get("matrix") if strategy is not None else None
    include = matrix.get("include") if matrix is not None else None
    for item in include.items if include is not None else []:
        for k in _MATRIX_KEYS:
            v = item.get(k)
            if v is None or not v.value:
                out.append(Problem(CI, item.line, "tiers", f"tier entry without `{k}`"))
    covered = _check_submakes(tree.makefile)
    want = [t for t in _make_prereqs(tree.makefile, "test") if t not in covered]
    if not want:
        out.append(Problem("Makefile", 1, "tiers", "no `test:` prerequisites found"))
    entries = _tier_entries(tier)
    for arch in sorted({e.arch for e in entries}):
        seen: dict[str, str] = {}
        for e in [x for x in entries if x.arch == arch]:
            for t in e.targets:
                if t not in want:
                    what = "is not a `make test` tier" if _is_rule(tree.makefile, t) else "unknown"
                    out.append(Problem(CI, e.line, "tiers", f"tier {e.tier}: target {t} {what}"))
                elif t in seen:
                    out.append(
                        Problem(CI, e.line, "tiers", f"{t} in tiers {seen[t]} and {e.tier}")
                    )
                else:
                    seen[t] = e.tier
        for t in want:
            if t not in seen:
                msg = f"{arch}: `make test` runs {t}, no tier does"
                out.append(Problem(CI, tier.line, "tiers", msg))
    return out


TIER_TABLE_HEADER = "| Arch | Tier | Targets | QEMU s |"


def section(md: str, number: str) -> tuple[int, str]:
    """(first line, text) of the `## <number> ` section of `md`, or (0, "")."""
    lines = md.splitlines()
    start = next((i for i, ln in enumerate(lines) if ln.startswith(f"## {number} ")), None)
    if start is None:
        return 0, ""
    end = next(
        (i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")), len(lines)
    )
    return start + 1, "\n".join(lines[start:end])


def rule_budget_doc(tree: Tree) -> list[Problem]:
    """L1199: DESIGN §8.6's tier table is the `tier` matrix."""
    doc = "docs/TESTING.md"
    first, text = section(tree.testing_md, "8.6")
    lines = text.splitlines()
    try:
        at = next(i for i, ln in enumerate(lines) if ln.strip() == TIER_TABLE_HEADER)
    except StopIteration:
        return [Problem(doc, first or 1, "budget_doc", f"§8.6 has no `{TIER_TABLE_HEADER}` table")]
    rows: dict[tuple[str, str], tuple[tuple[str, ...], int]] = {}
    out = []
    for i in range(at + 2, len(lines)):
        ln = lines[i].strip()
        if not ln.startswith("|"):
            break
        cells = [c.strip() for c in ln.strip("|").split("|")]
        no = first + i
        if len(cells) != 4 or not re.fullmatch(r"\d+", cells[3]):
            msg = "tier row is not `| <arch> | <tier> | <`target`s> | <seconds> |`"
            out.append(Problem(doc, no, "budget_doc", msg))
            continue
        targets = tuple(re.findall(r"`([^`]+)`", cells[2]))
        rows[(cells[0], cells[1])] = (targets, no)
    wf = tree.workflows.get(CI)
    tier = next((j for j in _jobs(wf) if j.key == "tier"), None) if wf is not None else None
    entries = _tier_entries(tier) if tier is not None else []
    for e in entries:
        row = rows.pop((e.arch, e.tier), None)
        if row is None:
            msg = f"no row for tier ({e.arch}, {e.tier})"
            out.append(Problem(doc, first + at, "budget_doc", msg))
        elif sorted(row[0]) != sorted(e.targets):
            out.append(
                Problem(
                    doc,
                    row[1],
                    "budget_doc",
                    f"tier ({e.arch}, {e.tier}) targets {list(row[0])}, ci.yml {list(e.targets)}",
                )
            )
    for (arch, name), (_, no) in rows.items():
        out.append(Problem(doc, no, "budget_doc", f"row ({arch}, {name}) is no ci.yml tier"))
    return out


UPSTREAM = "docs/UPSTREAM.md"
UPSTREAM_FIELDS = ("Reproducer", "Versions", "Workaround", "Upstream")
ENTRY_RE = re.compile(r"### [^:\s][^:]*: \S")


def _entries(md: str) -> list[tuple[int, str, str]]:
    """(line, heading, body) per `### ` entry, fenced blocks skipped."""
    out: list[tuple[int, str, list[str]]] = []
    fenced = False
    for no, ln in enumerate(md.splitlines(), 1):
        if ln.startswith("```"):
            fenced = not fenced
            continue
        if fenced:
            continue
        if ln.startswith("### "):
            out.append((no, ln, []))
        elif ln.startswith("#"):
            out.append((no, "", []))
        elif out:
            out[-1][2].append(ln)
    return [(no, h, "\n".join(body)) for no, h, body in out if h]


def rule_upstream(tree: Tree) -> list[Problem]:
    """L1245: a QEMU bug is worked around and its upstream report drafted, never retried."""
    doc = "docs/TESTING.md"
    first, text = section(tree.testing_md, "8.6")
    out = []
    if "](UPSTREAM.md" not in text:
        out.append(Problem(doc, first or 1, "upstream", "§8.6 does not link UPSTREAM.md"))
    if tree.upstream_md is None:
        return [*out, Problem(UPSTREAM, 1, "upstream", "docs/UPSTREAM.md is missing")]
    for no, heading, body in _entries(tree.upstream_md):
        if not ENTRY_RE.match(heading):
            msg = "entry heading is not `### <project>: <title>`"
            out.append(Problem(UPSTREAM, no, "upstream", msg))
        for f in UPSTREAM_FIELDS:
            if f"**{f}.**" not in body:
                out.append(Problem(UPSTREAM, no, "upstream", f"entry has no **{f}.** field"))
        m = re.search(r"\*\*Workaround\.\*\*\s*`([^`]+)`", body)
        if "**Workaround.**" in body:
            path = re.split(r"::|:", m.group(1))[0] if m else ""
            if not path or not (tree.root / path).exists():
                msg = f"Workaround names no in-tree path first ({path or 'none'})"
                out.append(Problem(UPSTREAM, no, "upstream", msg))
    return out


# L1200 (DESIGN §8.6, Scheduled capacity): the ten scheduled lanes.
RELEASE = f"{WORKFLOWS_DIR}/release.yml"
LANE_MAP_HEADER = "| Lane | Reserved for | Jobs |"
LEDGER_HEADER = (
    "| Workflow | Cadence | Jobs per run | Job-hours per run | Peak concurrent jobs | Lanes |"
)
LANE_RE = re.compile(r"sched-lane-[0-9]")
RESERVED = ("nightly", "weekly", "scheduled", "history", "rebuilds")
RESERVED_TIMEOUT_MIN = 330
LANE_CAPACITY = 100


@dataclass(frozen=True)
class LaneRow:
    """One lane-map row: its reservation and the workflow files its Jobs cell names."""

    reserved: str
    workflows: frozenset[str]
    line: int


@dataclass(frozen=True)
class LedgerRow:
    """One ledger row, keyed by workflow file name."""

    jobs_per_run: int | None
    lanes: tuple[str, ...]
    line: int


def _table(md: str, header: str) -> list[tuple[int, list[str]]]:
    """(1-based line, cells) per row of the table in §8.6 under exactly `header`."""
    first, text = section(md, "8.6")
    lines = text.splitlines()
    at = next((i for i, ln in enumerate(lines) if ln.strip() == header), None)
    if at is None:
        return []
    out = []
    for i in range(at + 2, len(lines)):
        ln = lines[i].strip()
        if not ln.startswith("|"):
            break
        out.append((first + i, [c.strip() for c in ln.strip("|").split("|")]))
    return out


def lane_map(text: str) -> dict[str, LaneRow]:
    """DESIGN §8.6's lane map: lane name to row."""
    out = {}
    for no, cells in _table(text, LANE_MAP_HEADER):
        lane = re.fullmatch(r"`?(sched-lane-[0-9])`?", cells[0]) if cells else None
        if lane is None or len(cells) != 3:
            continue
        wfs = frozenset(re.findall(r"`([\w.-]+\.ya?ml)`", cells[2]))
        out[lane.group(1)] = LaneRow(cells[1].strip("`"), wfs, no)
    return out


def ledger(text: str) -> dict[str, LedgerRow]:
    """DESIGN §8.6's ledger: workflow file name to row."""
    out = {}
    for no, cells in _table(text, LEDGER_HEADER):
        if len(cells) != 6:
            continue
        m = re.search(r"\d+", cells[2])
        out[cells[0].strip("`")] = LedgerRow(
            int(m.group(0)) if m else None, tuple(LANE_RE.findall(cells[5])), no
        )
    return out


def scheduled(tree: Tree) -> list[tuple[str, Node]]:
    """Workflows that run on a schedule or a dispatch, ci.yml and release.yml aside."""
    out = []
    for path, wf in tree.workflows.items():
        if path in (CI, RELEASE):
            continue
        on = wf.get("on")
        if on is None:
            continue
        events = {c.key for c in on.children} | set(on.scalars())
        if events & {"schedule", "workflow_dispatch"}:
            out.append((path, wf))
    return out


def _hosted(job: Node) -> bool:
    runs_on = job.get("runs-on")
    return runs_on is None or "self-hosted" not in runs_on.scalars()


def _lane(job: Node) -> str | None:
    """The job's literal `sched-lane-<n>` group, or None."""
    conc = job.get("concurrency")
    group = conc.get("group") if conc is not None and conc.kind == "map" else None
    value = group.value if group is not None and group.kind == "scalar" else None
    return value if value is not None and LANE_RE.fullmatch(value) else None


def _job_count(job: Node) -> int | None:
    """How many jobs one run starts for `job`: its matrix's literal axes multiplied,
    plus its `include` entries; None when the matrix uses an expression."""
    strategy = job.get("strategy")
    matrix = strategy.get("matrix") if strategy is not None else None
    if matrix is None:
        return 1
    if matrix.kind != "map":
        return None
    product = 1
    axes = 0
    includes = 0
    for axis in matrix.children:
        if axis.key == "exclude":
            continue
        if axis.kind != "seq":
            return None
        if axis.key == "include":
            includes = len(axis.items)
            continue
        if any(i.kind != "scalar" or "${{" in (i.value or "") for i in axis.items):
            return None
        product *= len(axis.items)
        axes += 1
    return (product if axes else 0) + includes


def _sched_jobs(tree: Tree) -> Iterator[tuple[str, Node, Node]]:
    for path, wf in scheduled(tree):
        for job in _jobs(wf):
            if _hosted(job):
                yield path, wf, job


def job_lanes(tree: Tree) -> dict[tuple[str, str], str]:
    """Each scheduled job's lane, keyed by (workflow path, the job's display name,
    its `name:` or else its id), for `ci_history.py --budget`."""
    out = {}
    for path, _, job in _sched_jobs(tree):
        lane = _lane(job)
        if lane is None or job.key is None:
            continue
        name = job.get("name")
        out[(path, name.value if name is not None and name.value else job.key)] = lane
    return out


def rule_ledger_row(tree: Tree) -> list[Problem]:
    """L1200: every scheduled or dispatched workflow has a DESIGN §8.6 ledger row."""
    rows = ledger(tree.testing_md)
    out = []
    for path, wf in scheduled(tree):
        if Path(path).name not in rows:
            msg = f"scheduled workflow {Path(path).name} has no §8.6 ledger row"
            out.append(Problem(path, wf.line, "ledger_row", msg))
    return out


def rule_lane_capacity(tree: Tree) -> list[Problem]:
    """L1200: a lane queues at most 100 jobs, so no run puts more than that in one lane."""
    out = []
    counts: dict[tuple[str, str], int] = {}
    for path, _, job in _sched_jobs(tree):
        lane = _lane(job)
        n = _job_count(job)
        if n is None:
            msg = f"job `{job.key}`: matrix uses an expression; its job count is uncountable"
            out.append(Problem(path, job.line, "lane_capacity", msg))
            continue
        if lane is not None:
            counts[(path, lane)] = counts.get((path, lane), 0) + n
    for (path, lane), n in sorted(counts.items()):
        if n > LANE_CAPACITY:
            msg = f"{n} jobs of one run in {lane}; a lane holds {LANE_CAPACITY}"
            out.append(Problem(path, 1, "lane_capacity", msg))
    doc = "docs/TESTING.md"
    for name, row in ledger(tree.testing_md).items():
        if row.jobs_per_run is not None and row.jobs_per_run > LANE_CAPACITY * len(row.lanes):
            msg = (
                f"ledger row {name}: {row.jobs_per_run} jobs per run in "
                f"{len(row.lanes)} lane(s) of {LANE_CAPACITY}"
            )
            out.append(Problem(doc, row.line, "lane_capacity", msg))
    return out


def rule_job_lane(tree: Tree) -> list[Problem]:
    """L1200: a scheduled job declares a literal `sched-lane-<n>` group with `queue: max`.

    GitHub rejects `cancel-in-progress` beside `queue: max`, and a reserved lane
    holds only jobs that end within 5.5 hours.
    """
    lanes = lane_map(tree.testing_md)
    out = []
    for path, _, job in _sched_jobs(tree):
        conc = job.get("concurrency")
        queue = conc.get("queue") if conc is not None and conc.kind == "map" else None
        lane = _lane(job)
        if lane is None or queue is None or queue.value != "max":
            want = "concurrency: {group: sched-lane-<n>, queue: max}"
            msg = f"job `{job.key}` declares no `{want}`"
            out.append(Problem(path, job.line, "job_lane", msg))
            continue
        assert conc is not None
        if conc.get("cancel-in-progress") is not None:
            msg = f"job `{job.key}`: `cancel-in-progress` with `queue: max`"
            out.append(Problem(path, job.line, "job_lane", msg))
        row = lanes.get(lane)
        if row is not None and row.reserved in RESERVED:
            t = job.get("timeout-minutes")
            v = t.value if t is not None else None
            if v is None or not v.isdigit() or int(v) > RESERVED_TIMEOUT_MIN:
                msg = (
                    f"job `{job.key}` in reserved {lane} needs `timeout-minutes` "
                    f"<= {RESERVED_TIMEOUT_MIN}"
                )
                out.append(Problem(path, job.line, "job_lane", msg))
    return out


def rule_row_lane(tree: Tree) -> list[Problem]:
    """L1200: a scheduled job's lane is one its workflow's ledger row gives it."""
    rows = ledger(tree.testing_md)
    out = []
    for path, _, job in _sched_jobs(tree):
        lane = _lane(job)
        row = rows.get(Path(path).name)
        if lane is None or row is None:
            continue
        if lane not in row.lanes:
            msg = f"job `{job.key}` runs in {lane}, not in its ledger row's lanes {list(row.lanes)}"
            out.append(Problem(path, job.line, "row_lane", msg))
    return out


def rule_lane_map(tree: Tree) -> list[Problem]:
    """L1200: every lane a ledger row gives a workflow is one the lane map lists for it."""
    doc = "docs/TESTING.md"
    lanes = lane_map(tree.testing_md)
    out = []
    for name, row in ledger(tree.testing_md).items():
        for lane in row.lanes:
            entry = lanes.get(lane)
            if entry is None or name not in entry.workflows:
                msg = f"ledger row {name} names {lane}, which the lane map does not give it"
                out.append(Problem(doc, row.line, "lane_map", msg))
    return out


# ROADMAP §10.1 (L1206, L1207): release.yml's shape. §14.6 adds `schedule` to
# the triggers and its signers to the privileged commands.
RELEASE_TRIGGERS = frozenset({"workflow_dispatch"})
# Commands a privileged job may run: none of them runs a workspace file.
PRIVILEGED_COMMANDS = frozenset({"cd", "sha256sum", "ls", "test", "echo", "printf", "cat"})
RELEASE_ACTION = "softprops/action-gh-release"
RELEASE_IMAGE = "vibeos.iso"
_ASSIGN_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=")
_SEP_RE = re.compile(r"&&|\|\||;|\|")


def _events(on: Node) -> list[tuple[str, Node]]:
    """(event, node) per trigger of an `on:` mapping, list or scalar."""
    if on.kind == "map":
        return [(c.key or "", c) for c in on.children]
    return [(s, on) for s in on.scalars()]


def _steps(job: Node) -> list[Node]:
    steps = job.get("steps")
    return steps.items if steps is not None else []


def _writes(perm: Node | None) -> bool:
    if perm is None:
        return False
    if perm.kind == "scalar":
        return perm.value == "write-all"
    return any(g.value == "write" for g in perm.children)


def _privileged(job: Node, top: Node | None) -> bool:
    """A write grant, its own or else the workflow's, or the `release`
    environment (ROADMAP §14.6)."""
    perm = job.get("permissions")
    env = job.get("environment")
    name = env.get("name") if env is not None and env.kind == "map" else env
    return _writes(perm if perm is not None else top) or (
        name is not None and name.value == "release"
    )


def shell_commands(script: str) -> list[str]:
    """The first word of each command in `script`, `NAME=value` words skipped."""
    out = []
    for line in script.replace("\\\n", " ").split("\n"):
        if line.strip().startswith("#"):
            continue
        for part in _SEP_RE.split(line):
            words = part.split()
            while words and _ASSIGN_RE.match(words[0]):
                words.pop(0)
            if words:
                out.append(words[0])
    return out


def rule_release_triggers(tree: Tree) -> list[Problem]:
    """L1207: release.yml's one trigger is `workflow_dispatch` with a required `tag`."""
    wf = tree.workflows.get(RELEASE)
    if wf is None:
        return []
    on = wf.get("on")
    if on is None:
        return [Problem(RELEASE, 1, "release_triggers", "no `on:`")]
    out = []
    for event, node in _events(on):
        if event not in RELEASE_TRIGGERS:
            msg = f"trigger `{event}`; release.yml runs only on `workflow_dispatch`"
            out.append(Problem(RELEASE, node.line, "release_triggers", msg))
    dispatch = on.get("workflow_dispatch") if on.kind == "map" else None
    inputs = dispatch.get("inputs") if dispatch is not None else None
    tag = inputs.get("tag") if inputs is not None else None
    required = tag.get("required") if tag is not None else None
    if required is None or required.value != "true":
        msg = "`workflow_dispatch` has no required `tag` input"
        out.append(Problem(RELEASE, (dispatch or on).line, "release_triggers", msg))
    return out


def rule_release_no_cache(tree: Tree) -> list[Problem]:
    """L1207: release.yml restores no cache, which could change what it builds."""
    wf = tree.workflows.get(RELEASE)
    out = []
    for n in wf.walk() if wf is not None else []:
        if n.key == "uses" and (n.value or "").startswith("actions/cache"):
            out.append(Problem(RELEASE, n.line, "release_no_cache", f"uses {n.value}"))
        if n.key == "with" and n.kind == "map" and n.get("cache") is not None:
            out.append(Problem(RELEASE, n.line, "release_no_cache", "`with:` sets `cache`"))
    return out


def rule_release_no_workflow_write(tree: Tree) -> list[Problem]:
    """L1207: release.yml grants no write workflow-wide; one job holds it."""
    wf = tree.workflows.get(RELEASE)
    top = wf.get("permissions") if wf is not None else None
    if top is None or not _writes(top):
        return []
    msg = "workflow-wide write grant; grant it to the publishing job alone"
    return [Problem(RELEASE, top.line, "release_no_workflow_write", msg)]


def rule_release_privileged_jobs(tree: Tree) -> list[Problem]:
    """L1207: a release.yml job with a write grant or the `release` environment
    checks out nothing and runs no repository script (ROADMAP §10.1, §14.6)."""
    wf = tree.workflows.get(RELEASE)
    out = []
    top = wf.get("permissions") if wf is not None else None
    for job in _jobs(wf) if wf is not None else []:
        if not _privileged(job, top):
            continue

        def bad(line: int, what: str, job: Node = job) -> None:
            msg = f"privileged job `{job.key}` {what}"
            out.append(Problem(RELEASE, line, "release_privileged_jobs", msg))

        uses = job.get("uses")
        if uses is not None and (uses.value or "").startswith("./"):
            bad(uses.line, f"uses {uses.value}")
        for step in _steps(job):
            u = step.get("uses")
            v = (u.value or "") if u is not None else ""
            if u is not None and v.startswith("actions/checkout"):
                bad(u.line, "checks out the repository")
            elif u is not None and v.startswith("./"):
                bad(u.line, f"uses the repository's {v}")
            run = step.get("run")
            if run is None or run.value is None:
                continue
            if "$(" in run.value or "`" in run.value:
                bad(run.line, "runs a command substitution")
            for cmd in shell_commands(run.value):
                if cmd not in PRIVILEGED_COMMANDS:
                    bad(run.line, f"runs `{cmd}`, outside PRIVILEGED_COMMANDS")
    return out


def rule_release_one_image(tree: Tree) -> list[Problem]:
    """L1206: the release publishes vibeos.iso as its one image, after `build`."""
    wf = tree.workflows.get(RELEASE)
    if wf is None:
        return []
    out = []
    seen = False
    for job in _jobs(wf):
        for step in _steps(job):
            u = step.get("uses")
            if u is None or not (u.value or "").startswith(RELEASE_ACTION):
                continue
            with_ = step.get("with")
            files = with_.get("files") if with_ is not None else None
            names = re.split(r"[\s,]+", files.value or "") if files is not None else []
            bases = [Path(n).name for n in names if n]
            for b in bases:
                if b.endswith(".iso") and b != RELEASE_IMAGE:
                    msg = f"publishes {b}; {RELEASE_IMAGE} is the one image"
                    out.append(Problem(RELEASE, (files or u).line, "release_one_image", msg))
            if RELEASE_IMAGE in bases:
                seen = True
            needs = job.get("needs")
            if needs is None or "build" not in needs.scalars():
                msg = f"publishing job `{job.key}` lacks `needs: build`"
                out.append(Problem(RELEASE, job.line, "release_one_image", msg))
    if not seen:
        msg = f"no {RELEASE_ACTION} step publishes {RELEASE_IMAGE}"
        out.append(Problem(RELEASE, 1, "release_one_image", msg))
    return out

RELEASE_PROFILE_RE = re.compile(r"\bCARGO_PROFILE=release(?![\w-])")
RELEASE_ARTIFACTS_RE = re.compile(r"\bmake release-artifacts OUT=(\S+)")


def _make_recipe(makefile: str, target: str) -> str:
    """The recipe lines of `target:` in `makefile`, joined; empty when none."""
    m = re.search(rf"^{re.escape(target)}:(?!=).*\n((?:\t.*\n)*)", makefile, re.M)
    return m.group(1) if m is not None else ""


def rule_release_profile(tree: Tree) -> list[Problem]:
    """L1268: v* releases ship the release profile. `make release-artifacts`
    builds with `CARGO_PROFILE=release`, and release.yml's `build` job runs
    it, then a `make CARGO_PROFILE=release test-e2e` step on that build, then
    a `cmp` of the tested ISO with the published one."""
    wf = tree.workflows.get(RELEASE)
    if wf is None:
        return []
    out = []
    recipe = [ln for ln in _make_recipe(tree.makefile, "release-artifacts").splitlines()
              if not ln.strip().startswith("#")]
    if not any(RELEASE_PROFILE_RE.search(ln) for ln in recipe):
        msg = "`release-artifacts` does not build with CARGO_PROFILE=release"
        out.append(Problem("Makefile", 1, "release_profile", msg))
    build = next((j for j in _jobs(wf) if j.key == "build"), None)
    runs = []
    for step in _steps(build) if build is not None else []:
        r = step.get("run")
        runs.append(r.value or "" if r is not None else "")
    line = build.line if build is not None else 1
    art = next(((i, m) for i, r in enumerate(runs)
                if (m := RELEASE_ARTIFACTS_RE.search(r)) is not None), None)
    if art is None:
        msg = "`build` runs no `make release-artifacts OUT=<dir>`"
        return [*out, Problem(RELEASE, line, "release_profile", msg)]
    at, m = art
    tested = next((i for i in range(at + 1, len(runs))
                   if "make CARGO_PROFILE=release test-e2e" in runs[i]), None)
    if tested is None:
        msg = "`build` tests no release-profile image after `make release-artifacts`"
        return [*out, Problem(RELEASE, line, "release_profile", msg)]
    cmp = f"cmp build/vibeos.iso {m.group(1).rstrip('/')}/vibeos.iso"
    if not any(cmp in runs[i] for i in range(tested + 1, len(runs))):
        msg = f"`build` runs no `{cmp}` after its tests"
        out.append(Problem(RELEASE, line, "release_profile", msg))
    return out


SECRET_EXPR_RE = re.compile(r"\$\{\{[^}]*\bsecrets\s*[.\[]")
# An upload path that holds a guest core, a memory dump or a QEMU command line.
CORE_PATH_RE = re.compile(
    r"(^|/)cores?(/|$)|\.core(\.zst)?$|vmcore|(^|/)[^/]*dump[^/]*(/|$)|qemu[-_]argv"
)
CORES_DIR = "build/cores"


def names_secret(wf: Node) -> bool:
    """An `environment:` key, `secrets.`/`secrets[` in an expression, or
    `secrets: inherit` anywhere in the workflow."""
    for n in wf.walk():
        if n.key == "environment":
            return True
        if n.key == "secrets" and n.value == "inherit":
            return True
        if n.value is not None and SECRET_EXPR_RE.search(n.value):
            return True
    return False


def _covers_cores(path: str) -> bool:
    """`path` is `build/cores`, a directory above it, or a glob matching either."""
    p = path.strip()
    while p.startswith("./"):
        p = p[2:]
    p = p.rstrip("/") or "."
    if p in (".", CORES_DIR) or CORES_DIR.startswith(p + "/"):
        return True
    return any(fnmatch.fnmatchcase(d, p) for d in (CORES_DIR, "build"))


def _core_paths(path: str, p: Node) -> list[Problem]:
    """A `path:` input's entries that may hold a core, a dump or a QEMU argv."""
    entries = (p.value or "").split("\n") if p.kind == "scalar" else p.scalars()
    out = []
    for entry in entries:
        e = entry.strip()
        if not e or e.startswith("!"):
            continue
        if CORE_PATH_RE.search(e) or _covers_cores(e):
            out.append(
                Problem(
                    path,
                    p.line,
                    "no_core_upload_with_secrets",
                    f"uploads {e!r}, which may hold a guest core, a memory dump or a QEMU "
                    "command line, in a workflow that names a secret or an environment",
                )
            )
    return out


def rule_no_core_upload_with_secrets(tree: Tree) -> list[Problem]:
    """L1410: a workflow that names an environment or a secret uploads no guest
    core, memory dump or QEMU command line (the core artifact is public, DESIGN §1.5)."""
    out = []
    for path, wf in tree.workflows.items():
        if not names_secret(wf):
            continue
        for job in _jobs(wf):
            steps = job.get("steps")
            for st in steps.items if steps is not None else []:
                uses = st.get("uses")
                if uses is None or not (uses.value or "").startswith("actions/upload-artifact@"):
                    continue
                with_ = st.get("with")
                p = with_.get("path") if with_ is not None else None
                if p is not None:
                    out += _core_paths(path, p)
    return out


RULES: list[Callable[[Tree], list[Problem]]] = [
    rule_no_expr_in_run,
    rule_permissions,
    rule_action_pins,
    rule_runs_on,
    rule_qemu_pin,
    rule_ci_triggers,
    rule_concurrency_group,
    rule_gate_dispatch,
    rule_tiers,
    rule_budget_doc,
    rule_upstream,
    rule_ledger_row,
    rule_lane_capacity,
    rule_job_lane,
    rule_row_lane,
    rule_lane_map,
    rule_release_triggers,
    rule_release_no_cache,
    rule_release_no_workflow_write,
    rule_release_privileged_jobs,
    rule_release_one_image,
    rule_release_profile,
    rule_no_core_upload_with_secrets,
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
