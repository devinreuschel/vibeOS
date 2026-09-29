#!/usr/bin/env python3
"""Adapted files carry a DESIGN §1.5 provenance header (ROADMAP §10.9).

A file adapted from a notice-only upstream keeps its notice and names where it
came from, within its first 40 lines, in one comment block (`//`, `#` or `;`
lines, or one `/* ... */` block with ` * ` prefixes):

    <c> Provenance: <https repository URL> <path in that repository> @ <tag or 40-hex commit>
    <c> Upstream-License: <SPDX expression, as upstream states it>
    <c> <upstream copyright line(s) and permission notice, verbatim, to the end of the block>

Scope: git-tracked files with a source suffix, and the `Makefile`, outside
`third_party/` and `LICENSE`; a file that is not UTF-8 is skipped. It fails
when a file has a copyright line (its first token, after an optional comment
marker, is the word or the sign) or an SPDX license line but no header; when a
header lacks a field or its notice; and when `Upstream-License` has no
OR-option made only of DESIGN §1.5's notice-only ids.

    check_provenance.py [--fetch]

`--fetch` (the nightly job) also fetches each recorded upstream file at its
pinned revision and fails unless its SPDX line names the recorded license or,
where it has none, its first comment block holds the text of a notice-only
option (whitespace and comment markers ignored). `gen_notices.py` imports
`parse_header`, the one header parser.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tempfile
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SOURCE_SUFFIXES = frozenset({
    ".rs", ".py", ".sh", ".S", ".s", ".asm", ".inc", ".ld", ".c", ".h", ".toml",
    ".yml", ".yaml", ".json", ".conf", ".cfg",
})
SOURCE_NAMES = frozenset({"Makefile"})
EXCLUDED_DIRS = ("third_party/",)
EXCLUDED_FILES = frozenset({"LICENSE"})

# DESIGN §1.5's notice-only licenses, as SPDX ids.
NOTICE_ONLY = ("MIT", "X11", "HPND", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib", "0BSD")
HEADER_WINDOW = 40

# Built so that no line of this file matches the scanner.
SPDX_TAG = "SPDX-" + "License-Identifier:"
COPYRIGHT_WORD = "Copy" + "right"
COPYRIGHT_SIGN = "©"

LINE_MARKERS = ("//", "#", ";")
# Markers a comment line may start with, longest first.
STRIP_MARKERS = ("/*", "*/", "//", "--", "#", "*", ";")

PROVENANCE = re.compile(r"^Provenance:\s*(\S+)\s+(\S+)\s+@\s+(\S+)\s*$")
UPSTREAM_LICENSE = re.compile(r"^Upstream-License:\s*(.*?)\s*$")
HEX40 = re.compile(r"^[0-9a-f]{40}$")
TAG = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._/+-]*$")


@dataclass(frozen=True)
class Header:
    """One provenance header. `line` is 1-based, the Provenance line's."""

    path: str
    line: int
    url: str
    upstream_path: str
    rev: str
    license: str
    notice: str


class HeaderError(Exception):
    """A malformed header: `line` (1-based) and what is wrong."""

    def __init__(self, line: int, message: str) -> None:
        super().__init__(f"{line}: {message}")
        self.line = line
        self.message = message


def _strip_marker(line: str) -> str:
    """The text of a comment line with one leading marker and one space removed."""
    s = line.strip()
    for m in STRIP_MARKERS:
        if s.startswith(m):
            s = s[len(m):]
            break
    if s.startswith(" "):
        s = s[1:]
    return s.rstrip()


def _comment_block(lines: list[str], i: int) -> tuple[list[tuple[int, str]], bool] | None:
    """The comment block that holds line `i`, as (index, text) pairs from
    line `i` to the block's end, and whether it is a `/* */` block. None when
    line `i` is not in a comment."""
    s = lines[i].lstrip()
    for m in LINE_MARKERS:
        if s.startswith(m):
            out = []
            j = i
            while j < len(lines) and lines[j].lstrip().startswith(m):
                out.append((j, _strip_marker(lines[j])))
                j += 1
            return out, False
    # Inside a /* */ block: an opening `/*` at or above line i, no `*/` between.
    for k in range(i, -1, -1):
        t = lines[k]
        if "*/" in t and k < i:
            return None
        if "/*" in t:
            break
    else:
        return None
    out = []
    j = i
    while j < len(lines):
        t = lines[j]
        end = "*/" in t
        text = t.split("*/", 1)[0]
        text = _strip_marker(text) if text.strip() else ""
        if text or not end:
            out.append((j, text))
        if end:
            break
        j += 1
    return out, True


def parse_header(text: str, path: str = "") -> Header | None:
    """The file's provenance header, None when its first 40 lines hold no
    `Provenance:` comment line; raises HeaderError when one is malformed."""
    lines = text.splitlines()
    for i, raw in enumerate(lines[:HEADER_WINDOW]):
        if "Provenance:" not in raw:
            continue
        found = _comment_block(lines, i)
        if found is None:
            continue
        block, _ = found
        first = block[0][1]
        if not first.startswith("Provenance:"):
            continue
        m = PROVENANCE.match(first)
        if not m:
            raise HeaderError(i + 1, "Provenance: wants "
                              "`<https URL> <path> @ <tag or 40-hex commit>`")
        url, upstream_path, rev = m.groups()
        if not url.startswith("https://"):
            raise HeaderError(i + 1, f"Provenance: URL {url} is not https")
        if not (HEX40.match(rev) or TAG.match(rev)):
            raise HeaderError(i + 1, f"Provenance: {rev} is not a tag or a 40-hex commit")
        if len(block) < 2:
            raise HeaderError(i + 1, "header lacks its Upstream-License line")
        lm = UPSTREAM_LICENSE.match(block[1][1])
        if not lm:
            raise HeaderError(block[1][0] + 1, "header lacks its Upstream-License line")
        lic = lm.group(1)
        if not lic:
            raise HeaderError(block[1][0] + 1, "Upstream-License is empty")
        notice = "\n".join(t for _, t in block[2:]).strip("\n")
        if not notice.strip():
            raise HeaderError(block[1][0] + 1, "header has no upstream notice "
                              "(DESIGN §1.5: the file keeps the notice)")
        return Header(path, i + 1, url, upstream_path, rev, lic, notice)
    return None


# SPDX expressions: ids, AND, OR, WITH, parentheses; `/` is the legacy OR.
_TOKEN = re.compile(r"\s*(\(|\)|/|[A-Za-z0-9.+:-]+)")


def _tokens(expr: str) -> list[str]:
    out = []
    pos = 0
    expr = expr.strip()
    while pos < len(expr):
        m = _TOKEN.match(expr, pos)
        if not m:
            raise ValueError(f"bad SPDX expression {expr!r}")
        out.append(m.group(1))
        pos = m.end()
        while pos < len(expr) and expr[pos].isspace():
            pos += 1
    return out


def options(expr: str) -> list[frozenset[str]]:
    """The OR-options of an SPDX expression, each the set of ids it needs.
    `X WITH Y` is one id, `X WITH Y`."""
    toks = _tokens(expr)
    pos = 0

    def peek() -> str | None:
        return toks[pos] if pos < len(toks) else None

    def take() -> str:
        nonlocal pos
        if pos >= len(toks):
            raise ValueError(f"bad SPDX expression {expr!r}")
        pos += 1
        return toks[pos - 1]

    def atom() -> list[frozenset[str]]:
        t = take()
        if t == "(":
            r = alt()
            if take() != ")":
                raise ValueError(f"bad SPDX expression {expr!r}")
            return r
        if t in (")", "/") or t.upper() in ("AND", "OR", "WITH"):
            raise ValueError(f"bad SPDX expression {expr!r}")
        nxt = peek()
        if nxt is not None and nxt.upper() == "WITH":
            take()
            t = f"{t} WITH {take()}"
        return [frozenset({t})]

    def conj() -> list[frozenset[str]]:
        r = atom()
        while (p := peek()) is not None and p.upper() == "AND":
            take()
            r = [a | b for a in r for b in atom()]
        return r

    def alt() -> list[frozenset[str]]:
        r = conj()
        while (p := peek()) is not None and (p.upper() == "OR" or p == "/"):
            take()
            r = r + conj()
        return r

    result = alt()
    if pos != len(toks):
        raise ValueError(f"bad SPDX expression {expr!r}")
    return result


def notice_only(expr: str) -> bool:
    """True when some OR-option of `expr` is made only of notice-only ids."""
    try:
        opts = options(expr)
    except ValueError:
        return False
    return any(opt and all(i in NOTICE_ONLY for i in opt) for opt in opts)


def normalize_expr(expr: str) -> str:
    """An SPDX expression with whitespace collapsed and operators upper-cased."""
    out = []
    for t in _tokens(expr):
        out.append(t.upper() if t.upper() in ("AND", "OR", "WITH") else t)
    return " ".join(out).replace("( ", "(").replace(" )", ")")


def source_files(root: Path = ROOT) -> list[str]:
    """Git-tracked files in scope, as repo-relative posix paths."""
    out = subprocess.run(["git", "-C", str(root), "ls-files", "-z"], check=True,
                         capture_output=True).stdout.decode()
    files = []
    for rel in sorted(p for p in out.split("\0") if p):
        if rel.startswith(EXCLUDED_DIRS) or rel in EXCLUDED_FILES:
            continue
        name = rel.rsplit("/", 1)[-1]
        if name in SOURCE_NAMES or Path(name).suffix in SOURCE_SUFFIXES:
            files.append(rel)
    return files


def _read(root: Path, rel: str) -> str | None:
    try:
        return (root / rel).read_bytes().decode("utf-8")
    except (UnicodeDecodeError, OSError):
        return None


def marked_lines(text: str) -> list[tuple[int, str]]:
    """(1-based line, what) for each copyright line and SPDX line."""
    out = []
    for i, line in enumerate(text.splitlines()):
        if SPDX_TAG in line:
            out.append((i + 1, "an SPDX license line"))
            continue
        s = line.strip()
        for m in STRIP_MARKERS:
            if s.startswith(m):
                s = s[len(m):].lstrip()
                break
        tok = s.split(maxsplit=1)[0] if s else ""
        if tok.rstrip(":") == COPYRIGHT_WORD or tok.startswith(COPYRIGHT_SIGN):
            out.append((i + 1, "a copyright line"))
    return out


def file_problems(rel: str, text: str) -> tuple[Header | None, list[str]]:
    """The file's header and its `path:line: message` problems."""
    try:
        header = parse_header(text, rel)
    except HeaderError as e:
        return None, [f"{rel}:{e.line}: {e.message}"]
    if header is None:
        return None, [f"{rel}:{line}: {what} but no provenance header (DESIGN §1.5)"
                      for line, what in marked_lines(text)[:1]]
    if not notice_only(header.license):
        return header, [f"{rel}:{header.line + 1}: Upstream-License {header.license!r} has no "
                        f"option made only of notice-only licenses ({', '.join(NOTICE_ONLY)}); "
                        "such code enters as a crate or a port (DESIGN §1.5)"]
    return header, []


def scan(root: Path = ROOT, files: Iterable[str] | None = None) -> tuple[list[Header], list[str]]:
    """(headers, problems) over the files in scope."""
    headers: list[Header] = []
    problems: list[str] = []
    for rel in (source_files(root) if files is None else files):
        text = _read(root, rel)
        if text is None:
            continue
        header, probs = file_problems(rel, text)
        if header is not None:
            headers.append(header)
        problems.extend(probs)
    return headers, problems


# The SPDX license-list bodies of the notice-only licenses, from the grant
# sentence to the end of the disclaimer (zlib's disclaimer comes first).
# `<>` matches upstream's variable text: a holder's name, a list marker, or
# a word SPDX marks optional.
TEMPLATES: dict[str, str] = {
    "MIT": """
Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:
The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.
THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL <> BE LIABLE FOR
ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
OTHER DEALINGS IN THE SOFTWARE.
""",
    "X11": """
Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:
The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.
THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL <> BE LIABLE FOR
ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
OTHER DEALINGS IN THE SOFTWARE.
Except as contained in this notice, the name of <> shall not be used in
advertising or otherwise to promote the sale, use or other dealings in this
Software without prior written authorization from <>.
""",
    "BSD-2-Clause": """
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:
<> Redistributions of source code must retain the above copyright notice, this
list of conditions and the following disclaimer.
<> Redistributions in binary form must reproduce the above copyright notice,
this list of conditions and the following disclaimer in the documentation
and/or other materials provided with the distribution.
THIS SOFTWARE IS PROVIDED BY <> "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND
FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL <> BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
""",
    "BSD-3-Clause": """
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:
<> Redistributions of source code must retain the above copyright notice, this
list of conditions and the following disclaimer.
<> Redistributions in binary form must reproduce the above copyright notice,
this list of conditions and the following disclaimer in the documentation
and/or other materials provided with the distribution.
<> Neither the name of <> nor the names of <> contributors may be used to
endorse or promote products derived from this software without specific prior
written permission.
THIS SOFTWARE IS PROVIDED BY <> "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND
FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL <> BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
""",
    "ISC": """
Permission to use, copy, modify, <> distribute this software for any purpose
with or without fee is hereby granted, provided that the above copyright notice
and this permission notice appear in all copies.
THE SOFTWARE IS PROVIDED "AS IS" AND <> DISCLAIMS ALL WARRANTIES WITH REGARD TO
THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS.
IN NO EVENT SHALL <> BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR
CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA
OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS
ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS
SOFTWARE.
""",
    "Zlib": """
This software is provided 'as-is', without any express or implied warranty. In
no event will the authors be held liable for any damages arising from the use of
this software.
Permission is granted to anyone to use this software for any purpose, including
commercial applications, and to alter it and redistribute it freely, subject to
the following restrictions:
<> The origin of this software must not be misrepresented; you must not claim
that you wrote the original software. If you use this software in a product, an
acknowledgment in the product documentation would be appreciated but is not
required.
<> Altered source versions must be plainly marked as such, and must not be
misrepresented as being the original software.
<> This notice may not be removed or altered from any source distribution.
""",
    "0BSD": """
Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted.
THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND
FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS
OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF
THIS SOFTWARE.
""",
}

_QUOTES = str.maketrans({"“": '"', "”": '"', "‘": "'", "’": "'"})


def flatten(text: str) -> str:
    """Text with comment markers dropped and whitespace collapsed to one space."""
    words = []
    for line in text.translate(_QUOTES).splitlines():
        s = line.strip()
        while True:
            for m in STRIP_MARKERS:
                if s.startswith(m):
                    s = s[len(m):].lstrip()
                    break
            else:
                break
        for m in ("*/", "-->"):
            if s.endswith(m):
                s = s[: -len(m)].rstrip()
        words.extend(s.split())
    return " ".join(words)


def template_regex(lic: str) -> re.Pattern[str]:
    parts = [re.escape(flatten(p)) for p in TEMPLATES[lic].split("<>")]
    return re.compile(r"\s?.{0,120}?\s?".join(parts), re.DOTALL)


def first_comment_block(text: str) -> str:
    """The first comment block of a file: its leading comment lines, or its
    first `/* */` block, whichever starts first."""
    lines = text.splitlines()
    for i, line in enumerate(lines):
        s = line.strip()
        if not s or s.startswith("#!"):
            continue
        if s.startswith("/*"):
            out = []
            for t in lines[i:]:
                out.append(t)
                if "*/" in t:
                    break
            return "\n".join(out)
        for m in ("//", "#", ";", "--"):
            if s.startswith(m):
                out = []
                for t in lines[i:]:
                    if not t.strip().startswith(m):
                        break
                    out.append(t)
                return "\n".join(out)
        return ""
    return ""


def upstream_problems(header: Header, text: str) -> list[str]:
    """Problems with a fetched upstream file against its header."""
    where = f"{header.path}:{header.line}"
    src = f"{header.url} {header.upstream_path} @ {header.rev}"
    for line in text.splitlines():
        if SPDX_TAG in line:
            got = line.split(SPDX_TAG, 1)[1]
            for m in ("*/", "-->"):
                got = got.split(m, 1)[0]
            got = got.strip()
            try:
                same = normalize_expr(got) == normalize_expr(header.license)
            except ValueError:
                same = False
            if same:
                return []
            return [f"{where}: {src} states {got!r}, the header records {header.license!r}"]
    block = flatten(first_comment_block(text))
    missing_text: set[str] = set()
    try:
        opts = options(header.license)
    except ValueError:
        return [f"{where}: Upstream-License {header.license!r} is not an SPDX expression"]
    for opt in opts:
        if not opt or not all(i in NOTICE_ONLY for i in opt):
            continue
        lacking = sorted(i for i in opt if i not in TEMPLATES)
        if lacking:
            missing_text.update(lacking)
            continue
        if all(template_regex(i).search(block) for i in opt):
            return []
    if missing_text:
        return [f"{where}: {src} has no SPDX line, and this script keeps no license text "
                f"for {', '.join(sorted(missing_text))} to compare"]
    return [f"{where}: {src} has no SPDX line, and its first comment block does not hold "
            f"the text of {header.license!r}"]


Fetch = Callable[[str, str, str], str]


def git_fetch(url: str, rev: str, path: str) -> str:
    """`path` at `rev` of the repository at `url`, fetched shallow into a
    temporary directory."""
    with tempfile.TemporaryDirectory() as d:
        def git(*args: str) -> bytes:
            return subprocess.run(["git", "-C", d, *args], check=True,
                                  capture_output=True).stdout
        git("init", "-q")
        git("fetch", "-q", "--depth", "1", url, rev)
        return git("show", f"FETCH_HEAD:{path}").decode("utf-8", errors="replace")


def fetch_check(headers: Iterable[Header], fetch: Fetch = git_fetch) -> list[str]:
    """Problems with each header's upstream file, fetched through `fetch`."""
    problems = []
    for h in headers:
        try:
            text = fetch(h.url, h.rev, h.upstream_path)
        except (subprocess.CalledProcessError, OSError) as e:
            problems.append(f"{h.path}:{h.line}: cannot fetch "
                            f"{h.url} {h.upstream_path} @ {h.rev}: {e}")
            continue
        problems.extend(upstream_problems(h, text))
    return problems


def main(argv: list[str] | None = None, root: Path = ROOT, fetch: Fetch = git_fetch) -> int:
    ap = argparse.ArgumentParser(prog="check_provenance.py")
    ap.add_argument("--fetch", action="store_true",
                    help="also check each header's upstream file at its pinned revision")
    args = ap.parse_args(argv if argv is not None else [])
    headers, problems = scan(root)
    if args.fetch and not problems:
        problems = fetch_check(headers, fetch)
    for p in problems:
        print(p, file=sys.stderr)
    if problems:
        return 1
    fetched = f", {len(headers)} fetched" if args.fetch else ""
    print(f"check_provenance: ok ({len(headers)} provenance headers{fetched})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
