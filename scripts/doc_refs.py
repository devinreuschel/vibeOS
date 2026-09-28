#!/usr/bin/env python3
"""Resolve `DESIGN §x.y` and `ROADMAP §x.y` citations across the tree (ROADMAP §10.3, DOC2).

DESIGN.md is the design's index: it keeps the preamble and §1, and §2 to §12 live in the topic
files its Contents table names (LAYOUT). Each heading keeps its DESIGN number, so `DESIGN §x.y`
names one section wherever it lives, and DESIGN §1.4 asks that the docs and the code agree. This
script checks, over every tracked file outside the dated records (EXCLUDE):

- the layout: each LAYOUT file exists, has its `# N.` heading, and holds only §N's headings, no
  number is defined twice, and DESIGN.md's Contents links each topic file;
- every `DESIGN §n`, `DESIGN.md §n`, `<TOPIC>.md §n`, `ROADMAP §n` and `ROADMAP.md §n`
  citation, including one wrapped across lines, names a heading that exists;
- every Markdown link into a LAYOUT file whose fragment names a heading of that file.

`--where x.y` prints the file and anchor that hold DESIGN §x.y.
"""

from __future__ import annotations

import argparse
import posixpath
import re
import subprocess
import sys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

LAYOUT: tuple[tuple[str, int], ...] = (
    ("docs/DESIGN.md", 1),
    ("docs/INVARIANTS.md", 2),
    ("docs/BOOT.md", 3),
    ("docs/MEMORY.md", 4),
    ("docs/INTERRUPTS.md", 5),
    ("docs/TIME.md", 6),
    ("docs/SMP.md", 7),
    ("docs/TESTING.md", 8),
    ("docs/PITFALLS.md", 9),
    ("docs/BLOCK.md", 10),
    ("docs/PORTABILITY.md", 11),
    ("docs/DEVICES.md", 12),
)
DESIGN = LAYOUT[0][0]
ROADMAP = "docs/ROADMAP.md"
EXCLUDE = ("docs/reviews/", "CHANGELOG.md")

TOPICS = "|".join(posixpath.basename(p)[: -len(".md")] for p, _ in LAYOUT[1:])
WRAP = r"(?:[ \t ]+|[ \t]*\n[ \t]*(?:(?://[/!]?|#|\*|;|--)[ \t]*)?)"
CITE = re.compile(rf"\b(DESIGN|ROADMAP|{TOPICS})(\.md)?{WRAP}§(\d+(?:\.\d+)*)")

HEADING = re.compile(r"^(#{1,6})[ \t]+(.+?)[ \t]*#*[ \t]*$")
NUMBER = re.compile(r"^(\d+(?:\.\d+)*)\.?(?=\s|$)")
FENCE = ("```", "~~~")
LINK = re.compile(r"\]\(([^)\s]*)")
PHASE = re.compile(r"^Phase (\d+):")
ROADMAP_SUB = re.compile(r"^(\d+\.\d+) ")


@dataclass(frozen=True)
class Heading:
    line: int
    level: int
    number: str | None
    slug: str


def slug(title: str) -> str:
    """GitHub's anchor for a heading title."""
    return re.sub(r"[^\w\- ]", "", title.strip().lower()).replace(" ", "-")


def headings(text: str) -> list[Heading]:
    """ATX headings outside fences, with GitHub's `-1`, `-2` suffix on a repeated slug."""
    out: list[Heading] = []
    seen: dict[str, int] = {}
    fenced = False
    for i, line in enumerate(text.splitlines(), start=1):
        if line.startswith(FENCE):
            fenced = not fenced
            continue
        if fenced:
            continue
        m = HEADING.match(line)
        if m is None:
            continue
        title = m.group(2)
        num = NUMBER.match(title)
        base = slug(title)
        count = seen.get(base, 0)
        seen[base] = count + 1
        out.append(
            Heading(
                line=i,
                level=len(m.group(1)),
                number=num.group(1) if num else None,
                slug=base if count == 0 else f"{base}-{count}",
            )
        )
    return out


def _layout_file(top: str) -> str | None:
    for path, n in LAYOUT:
        if str(n) == top:
            return path
    return None


def design_index(files: Mapping[str, str]) -> tuple[dict[str, tuple[str, str]], list[str]]:
    """Map each DESIGN section number to (path, slug) over LAYOUT, with layout errors."""
    index: dict[str, tuple[str, str]] = {}
    where_at: dict[str, tuple[str, int]] = {}
    errors: list[str] = []
    for path, n in LAYOUT:
        text = files.get(path)
        if text is None:
            errors.append(f"{path}: missing (DESIGN Contents names it)")
            continue
        hs = headings(text)
        if not any(h.level == 1 and h.number == str(n) for h in hs):
            errors.append(f'{path}: no top heading "# {n}. <title>"')
        for h in hs:
            if h.number is None:
                continue
            top = h.number.split(".")[0]
            if top != str(n):
                home = _layout_file(top) or "no file"
                errors.append(
                    f"{path}:{h.line}: §{h.number} is in the wrong file: "
                    f"DESIGN Contents puts §{top} in {home}"
                )
            if h.number in where_at:
                first, first_line = where_at[h.number]
                errors.append(f"{path}:{h.line}: §{h.number} also defined at {first}:{first_line}")
                continue
            where_at[h.number] = (path, h.line)
            index[h.number] = (path, h.slug)
    design = files.get(DESIGN)
    if design is not None:
        for path, _ in LAYOUT[1:]:
            name = re.escape(posixpath.basename(path))
            if re.search(rf"\]\({name}(?:#[^)]*)?\)", design) is None:
                errors.append(f"{DESIGN}: Contents does not link {path}")
    return index, errors


def roadmap_index(text: str) -> dict[str, str]:
    """Map `## Phase N:` to "N" and `### N.M ` to "N.M", each to its slug."""
    index: dict[str, str] = {}
    lines = text.splitlines()
    for h in headings(text):
        m = HEADING.match(lines[h.line - 1])
        title = m.group(2) if m else ""
        if h.level == 2:
            m = PHASE.match(title)
            if m:
                index.setdefault(m.group(1), h.slug)
        elif h.level == 3:
            m = ROADMAP_SUB.match(title)
            if m:
                index.setdefault(m.group(1), h.slug)
    return index


def _line_of(text: str, pos: int) -> int:
    return text.count("\n", 0, pos) + 1


def _anchors(files: Mapping[str, str]) -> dict[str, list[str]]:
    return {path: [h.slug for h in headings(files[path])] for path, _ in LAYOUT if path in files}


def check_citations(
    path: str,
    text: str,
    design: Mapping[str, tuple[str, str]],
    roadmap: Mapping[str, str],
) -> list[str]:
    """Report each DESIGN, topic-file or ROADMAP § citation that names no heading."""
    errors: list[str] = []
    for m in CITE.finditer(text):
        doc, md, num = m.group(1), m.group(2), m.group(3)
        line = _line_of(text, m.start())
        if doc == "DESIGN":
            if num not in design:
                errors.append(f"{path}:{line}: DESIGN §{num}: no such section in the DESIGN files")
        elif doc == "ROADMAP":
            if num not in roadmap:
                errors.append(f"{path}:{line}: ROADMAP §{num}: no such section in {ROADMAP}")
        elif md:
            want = f"docs/{doc}.md"
            found = design.get(num)
            if found is None or found[0] != want:
                hint = f" (it is in {found[0]})" if found else ""
                errors.append(f"{path}:{line}: {doc}.md §{num}: not in {want}{hint}")
    return errors


def check_links(path: str, text: str, anchors: Mapping[str, Sequence[str]]) -> list[str]:
    """Report each link into a LAYOUT file whose fragment names no heading of that file."""
    errors: list[str] = []
    base = posixpath.dirname(path)
    for m in LINK.finditer(text):
        target = m.group(1)
        if "://" in target or target.startswith("mailto:") or "#" not in target:
            continue
        file_part, frag = target.split("#", 1)
        resolved = path if file_part == "" else posixpath.normpath(posixpath.join(base, file_part))
        if resolved not in anchors or not frag:
            continue
        if frag in anchors[resolved]:
            continue
        others = [p for p, slugs in anchors.items() if p != resolved and frag in slugs]
        hint = f" (it is in {others[0]})" if others else ""
        line = _line_of(text, m.start())
        errors.append(f"{path}:{line}: link {target}: {resolved} has no heading #{frag}{hint}")
    return errors


def in_scope(path: str) -> bool:
    return not path.startswith(EXCLUDE)


def check_tree(files: Mapping[str, str]) -> list[str]:
    """Layout, citation and link errors over a tree given as path -> text, sorted."""
    design, errors = design_index(files)
    roadmap = roadmap_index(files.get(ROADMAP, ""))
    anchors = _anchors(files)
    for path, text in files.items():
        if not in_scope(path):
            continue
        errors.extend(check_citations(path, text, design, roadmap))
        if path.endswith(".md"):
            errors.extend(check_links(path, text, anchors))
    return sorted(errors)


def where(files: Mapping[str, str], number: str) -> str | None:
    """The file and anchor that hold DESIGN §number, as `docs/<FILE>.md#<slug>`."""
    found = design_index(files)[0].get(number)
    return f"{found[0]}#{found[1]}" if found else None


def tracked_files() -> dict[str, str]:
    """Every tracked file that decodes as UTF-8, read from the working tree."""
    out = subprocess.run(
        ["git", "ls-files", "-z"], cwd=ROOT, check=True, capture_output=True
    ).stdout
    files: dict[str, str] = {}
    for raw in out.split(b"\0"):
        if not raw:
            continue
        path = raw.decode("utf-8", errors="surrogateescape")
        try:
            files[path] = (ROOT / path).read_bytes().decode("utf-8")
        except (OSError, UnicodeDecodeError):
            continue
    return files


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0] if __doc__ else None)
    parser.add_argument("--where", metavar="X.Y", help="print the file that holds DESIGN §X.Y")
    args = parser.parse_args(argv)
    try:
        files = tracked_files()
    except (OSError, subprocess.CalledProcessError) as e:
        print(f"doc_refs: git ls-files failed: {e}", file=sys.stderr)
        return 2
    if args.where is not None:
        found = where(files, args.where)
        if found is None:
            print(f"doc_refs: no DESIGN §{args.where}")
            return 1
        print(found)
        return 0
    errors = check_tree(files)
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print("doc_refs: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
