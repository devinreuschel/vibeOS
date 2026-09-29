#!/usr/bin/env python3
"""The Funded goals section keeps its shape and its quotations resolve (ROADMAP §10.9).

The section runs from `# Funded goals` to the end of docs/ROADMAP.md. Its preamble runs to
the first `## `; a goal is a `### ` heading and its text, up to the next `###` or `##`.

- Each goal has a **Buy.**, **Rent.**, or **Open.** part, and its **Cost.**, **Unlocks.**,
  and **Lines.** parts, each starting anywhere in the goal. Its Lines run from **Lines.** to
  the next part or the goal's end.
- Each `"X" becomes` and `"X" gains` in a Lines bullet (a `- ` line and its indented
  continuations, not a `- [ ]` line) or in a preamble paragraph names a phrase X that occurs,
  with whitespace collapsed, exactly once in the file outside every goal's Lines and outside
  the preamble's double-quoted spans, and not on a checkbox line. A bullet that names an
  earlier goal (`the <words> goal`, `**` stripped, `<words>` part of that goal's heading in
  any case) whose Lines hold X passes instead.
- With code spans stripped, each item of a `(needs: ...)` in the section, split on `,` and
  ` and `, equals a goal's heading or an italic term of the **Names.** paragraph, ignoring
  case and a leading "the".
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts.gatelib import BOX, ROADMAP  # noqa: E402

SECTION = "# Funded goals"
PARTS = ("Buy", "Rent", "Open", "Cost", "Unlocks", "Lines")
PART = re.compile(r"\*\*(" + "|".join(PARTS) + r")\.\*\*")
QUOTE = re.compile(r'"([^"]*)"')
EDIT = re.compile(r"\s+(becomes|gains)\b")
NAMES_GOAL = re.compile(r"(?i)(?=\bthe (.+?) goal\b)")
CODE_SPAN = re.compile(r"(`+).+?\1")
NEEDS = re.compile(r"\(needs: ([^)]*)\)")
ITALIC = re.compile(r"(?<!\*)\*([^*]+)\*(?!\*)")


@dataclass
class Goal:
    heading: str
    start: int  # 0-based line index of the heading in the file
    end: int  # exclusive
    lines_spans: list[tuple[int, int]]  # [start, end) line ranges of its Lines part


def collapse(text: str) -> str:
    return " ".join(text.split())


def bare(name: str) -> str:
    """Casefolded, whitespace collapsed, with a leading "the" and `**` dropped."""
    s = collapse(name.replace("**", "")).casefold()
    return s[4:] if s.startswith("the ") else s


def parse_section(lines: list[str]) -> tuple[int, int, list[Goal]]:
    """(section start, preamble end, goals); line indexes are 0-based."""
    start = next((i for i, raw in enumerate(lines) if raw.rstrip() == SECTION), len(lines))
    pre_end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")),
                   len(lines))
    goals: list[Goal] = []
    i = pre_end
    while i < len(lines):
        if not lines[i].startswith("### "):
            i += 1
            continue
        j = i + 1
        while j < len(lines) and not lines[j].startswith(("### ", "## ")):
            j += 1
        spans: list[tuple[int, int]] = []
        k = i + 1
        while k < j:
            m = PART.search(lines[k])
            if m is not None and m.group(1) == "Lines":
                e = k + 1
                while e < j and PART.search(lines[e]) is None:
                    e += 1
                spans.append((k, e))
                k = e
                continue
            k += 1
        goals.append(Goal(lines[i][4:].strip(), i, j, spans))
        i = j
    return start, pre_end, goals


def bullets(lines: list[str], first: int, end: int) -> list[tuple[int, str]]:
    """(line index, text) of each `- ` bullet with its indented continuations."""
    out: list[tuple[int, str]] = []
    i = first
    while i < end:
        raw = lines[i]
        if raw.startswith("- ") and not BOX.match(raw):
            j = i + 1
            while j < end and lines[j].startswith((" ", "\t")) and lines[j].strip():
                j += 1
            out.append((i, "\n".join(lines[i:j])))
            i = j
            continue
        i += 1
    return out


def paragraphs(lines: list[str], first: int, end: int) -> list[tuple[int, str]]:
    out: list[tuple[int, str]] = []
    i = first
    while i < end:
        if not lines[i].strip():
            i += 1
            continue
        j = i
        while j < end and lines[j].strip():
            j += 1
        out.append((i, "\n".join(lines[i:j])))
        i = j
    return out


def edited_phrases(text: str) -> list[str]:
    """X of each `"X" becomes` and `"X" gains` in `text`, quotes paired left to right."""
    out: list[str] = []
    for m in QUOTE.finditer(text):
        if EDIT.match(text, m.end()):
            out.append(collapse(m.group(1)))
    return out


def searchable(lines: list[str], pre: tuple[int, int], goals: list[Goal]) -> tuple[str, list[int]]:
    """The file with every goal's Lines and the preamble's quoted spans masked, whitespace
    collapsed, and for each character of it the 0-based line it came from."""
    masked = list(lines)
    for g in goals:
        for a, b in g.lines_spans:
            for k in range(a, b):
                masked[k] = "\0"
    for first, para in paragraphs(lines, *pre):
        cut = QUOTE.sub(lambda m: "\0" + "\n" * m.group(0).count("\n"), para)
        for k, raw in enumerate(cut.split("\n")):
            masked[first + k] = raw
    chars: list[str] = []
    where: list[int] = []
    for n, raw in enumerate(masked):
        for c in raw + "\n":
            if c.isspace():
                if chars and chars[-1] != " ":
                    chars.append(" ")
                    where.append(n)
                continue
            chars.append(c)
            where.append(n)
    return "".join(chars), where


def occurrences(hay: str, needle: str) -> list[int]:
    out: list[int] = []
    at = hay.find(needle)
    while at >= 0 and needle:
        out.append(at)
        at = hay.find(needle, at + 1)
    return out


def names_terms(lines: list[str], pre: tuple[int, int]) -> set[str]:
    for _, para in paragraphs(lines, *pre):
        if para.startswith("**Names.**"):
            body = collapse(para[len("**Names.**"):])
            return {bare(t) for t in ITALIC.findall(body)}
    return set()


def check_funded(text: str) -> list[str]:
    errors: list[str] = []
    lines = text.splitlines()
    start, pre_end, goals = parse_section(lines)
    if start == len(lines):
        return [f"ROADMAP.md: no `{SECTION}` section"]
    pre = (start + 1, pre_end)
    for g in goals:
        body = "\n".join(lines[g.start:g.end])
        have = set(PART.findall(body))
        where = f"ROADMAP.md:{g.start + 1}: {g.heading}"
        if not have & {"Buy", "Rent", "Open"}:
            errors.append(f"{where}: no **Buy.**, **Rent.**, or **Open.** part")
        for p in ("Cost", "Unlocks", "Lines"):
            if p not in have:
                errors.append(f"{where}: no **{p}.** part")
    hay, where_line = searchable(lines, pre, goals)
    lines_text = [collapse("\n".join("\n".join(lines[a:b]) for a, b in g.lines_spans))
                  for g in goals]
    edits: list[tuple[int, str, int | None]] = []  # (line, text, index of its goal)
    for n, para in paragraphs(lines, *pre):
        edits.append((n, para, None))
    for gi, g in enumerate(goals):
        for a, b in g.lines_spans:
            edits.extend((n, t, gi) for n, t in bullets(lines, a, b))
    for n, t, owner in edits:
        for phrase in edited_phrases(t):
            if owner is not None and _earlier_goal_holds(t, phrase, goals[:owner], lines_text):
                continue
            hits = occurrences(hay, phrase)
            at = f"ROADMAP.md:{n + 1}"
            if len(hits) != 1:
                errors.append(f'{at}: "{phrase}" occurs {len(hits)} times outside the Lines '
                              f"and the preamble's quotations, not once")
                continue
            line = where_line[hits[0]]
            if BOX.match(lines[line]):
                errors.append(f'{at}: "{phrase}" occurs on a checkbox line, '
                              f"ROADMAP.md:{line + 1}")
    goal_names = {bare(g.heading) for g in goals}
    terms = names_terms(lines, pre)
    for k in range(start, len(lines)):
        for m in NEEDS.finditer(CODE_SPAN.sub("", lines[k])):
            for item in re.split(r",| and ", m.group(1)):
                name = bare(item)
                if name and name not in goal_names and name not in terms:
                    errors.append(f"ROADMAP.md:{k + 1}: (needs: {item.strip()}) names neither "
                                  f"a goal nor a machine the **Names.** paragraph lists")
    return errors


def _earlier_goal_holds(bullet: str, phrase: str, earlier: list[Goal],
                        lines_text: list[str]) -> bool:
    text = collapse(bullet.replace("**", "")).casefold()
    for m in NAMES_GOAL.finditer(text):
        words = m.group(1)
        for gi, g in enumerate(earlier):
            if words in g.heading.casefold() and phrase in lines_text[gi]:
                return True
    return False


def main(argv: list[str] | None = None) -> int:
    del argv
    errors = check_funded(ROADMAP.read_text(encoding="utf-8"))
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("check_funded: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
