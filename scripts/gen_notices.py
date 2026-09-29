#!/usr/bin/env python3
"""Write THIRD-PARTY-NOTICES.txt for an image (ROADMAP §10.9, DESIGN §1.5).

    gen_notices.py --out FILE [--arch x86_64] [--limine-dir DIR]

The file holds, in this order, each part under one marker line:

- `== vibeOS ==`: the tree's LICENSE.
- `== Limine <release> (binary <commit>) ==`: Limine's LICENSE, then one
  `-- Limine third-party: <name> (<license>) --` per project Limine's
  3RDPARTY.md lists, from `third_party/limine/` (MANIFEST.toml).
- `== Rust crates (<arch>) ==`: one `-- crate: <name> <version> (<license>) --`
  per crate of the normal dependency graph of each shipped root, with its
  license files, each after a `[<file>]` line, read from the registry source
  or `third_party/crates/<name>-<version>/`.
- `== Rust standard library: rustc <release> (<commit-hash>) ==`: the pinned
  toolchain's `share/doc/rust/COPYRIGHT-library.html` as text, then one
  `-- rust licenses/<ID>.txt --` per license its in-tree section names.
- `== Adapted in-tree files ==`: one `-- in-tree: <path> from <url> <upstream
  path> @ <rev> (<license>) --` per DESIGN §1.5 provenance header, with its
  notice (`check_provenance.parse_header`).

It fails, and writes nothing, when a crate of the graph has no license text,
comes from a source other than crates.io, or a workspace member is neither a
shipped root, reachable from one, nor host-only; when `third_party/limine/`
names a release or binary commit other than setup.sh's LIMINE_TAG and
LIMINE_COMMIT, lacks a project 3RDPARTY.md lists, or its LICENSE differs from
the clone's; when Rust's notice or a license text it names is missing; and
when a marker line it expects is absent. The output names no host path.
"""

from __future__ import annotations

import argparse
import html
import html.parser
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import check_provenance  # noqa: E402

LIMINE_TP = "third_party/limine"
CRATES_TP = "third_party/crates"

# The shipped roots per arch: (workspace member, target triple). A root that
# is not a workspace member (yet) is skipped.
ROOTS: dict[str, tuple[tuple[str, str], ...]] = {
    "x86_64": (
        ("vibeos", "x86_64-unknown-none"),
        ("vibeos-user", "x86_64-unknown-linux-musl"),
    ),
}
# Workspace members that ship in no image.
HOST_ONLY = frozenset({"vibeos-hostlib-tests"})

CRATES_IO = frozenset({
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
})
LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "copyright", "unlicense")
SPDX_OPERATORS = frozenset({"AND", "OR", "WITH"})


class NoticeError(Exception):
    """One reason the notices cannot be written."""


@dataclass(frozen=True)
class Crate:
    name: str
    version: str
    license: str
    source: str
    manifest_path: str
    license_file: str | None


# ---- Limine ----------------------------------------------------------------

_PIN = re.compile(r'^(LIMINE_TAG|LIMINE_COMMIT)="\$\{\1:-([^}]*)\}"\s*$', re.M)


def setup_pins(root: Path = ROOT) -> tuple[str, str]:
    """(LIMINE_TAG, LIMINE_COMMIT): the `${VAR:-default}` defaults in
    setup.sh, never the environment."""
    pins = dict(_PIN.findall((root / "setup.sh").read_text(encoding="utf-8")))
    if "LIMINE_TAG" not in pins or "LIMINE_COMMIT" not in pins:
        raise NoticeError("setup.sh: no LIMINE_TAG or LIMINE_COMMIT default")
    return pins["LIMINE_TAG"], pins["LIMINE_COMMIT"]


def limine_manifest(tp: Path) -> dict[str, Any]:
    with open(tp / "MANIFEST.toml", "rb") as f:
        return tomllib.load(f)


_ENTRY = re.compile(r"^- \[([^\]]+)\]\([^)]*\)\s*\(([^)]*)\)", re.S)


def parse_3rdparty(text: str) -> list[tuple[str, str]]:
    """(name, license as the file states it) for each top-level `- [name](url)
    (license)` entry of Limine's 3RDPARTY.md."""
    entries = []
    para: list[str] = []
    for line in text.splitlines() + [""]:
        if line.startswith("- ") or not line.strip():
            if para:
                m = _ENTRY.match(" ".join(para))
                if m:
                    entries.append((m.group(1), " ".join(m.group(2).split())))
            para = [line] if line.startswith("- ") else []
        elif para:
            para.append(line.strip())
    return entries


def check_limine(root: Path = ROOT, limine_dir: Path | None = None) -> list[str]:
    """Problems with `third_party/limine/` against setup.sh's pins, its
    3RDPARTY.md, and the clone's LICENSE when `limine_dir` holds one."""
    tp = root / LIMINE_TP
    problems = []
    tag, commit = setup_pins(root)
    try:
        man = limine_manifest(tp)
    except (OSError, tomllib.TOMLDecodeError) as e:
        return [f"{LIMINE_TP}/MANIFEST.toml: {e}"]
    want = tag.removesuffix("-binary")
    if man.get("release") != want:
        problems.append(f"{LIMINE_TP}/MANIFEST.toml: release {man.get('release')!r}, but "
                        f"setup.sh's LIMINE_TAG {tag} is built from {want}: "
                        "copy that release's texts")
    if man.get("binary_commit") != commit:
        problems.append(f"{LIMINE_TP}/MANIFEST.toml: binary_commit {man.get('binary_commit')!r}, "
                        f"but setup.sh's LIMINE_COMMIT is {commit}")
    projects = {p.get("name"): p for p in man.get("project", [])}
    try:
        listed = parse_3rdparty((tp / "3RDPARTY.md").read_text(encoding="utf-8"))
    except OSError as e:
        return problems + [f"{LIMINE_TP}/3RDPARTY.md: {e}"]
    if not listed:
        problems.append(f"{LIMINE_TP}/3RDPARTY.md lists no project")
    for name, _ in listed:
        if name not in projects:
            problems.append(f"{LIMINE_TP}/MANIFEST.toml: no [[project]] for 3RDPARTY.md's {name!r}")
    names = {n for n, _ in listed}
    for name, p in projects.items():
        if name not in names:
            problems.append(f"{LIMINE_TP}/MANIFEST.toml: project {name!r} is not in 3RDPARTY.md")
        if not p.get("license") or not p.get("files"):
            problems.append(f"{LIMINE_TP}/MANIFEST.toml: project {name!r} lacks license or files")
        for f in p.get("files", []):
            if not (tp / f).is_file():
                problems.append(f"{LIMINE_TP}/{f}: missing (project {name!r})")
    if not (tp / "LICENSE").is_file():
        problems.append(f"{LIMINE_TP}/LICENSE: missing")
    elif limine_dir is not None and (limine_dir / "LICENSE").is_file():
        if (limine_dir / "LICENSE").read_bytes() != (tp / "LICENSE").read_bytes():
            problems.append(f"{LIMINE_TP}/LICENSE differs from the Limine clone's LICENSE: "
                            "copy the pinned release's texts again")
    return problems


# ---- crates ----------------------------------------------------------------

def cargo_metadata(root: Path, triple: str) -> dict[str, Any]:
    """`cargo metadata` for every feature, filtered to `triple`."""
    import json

    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--all-features",
         "--filter-platform", triple],
        cwd=root, check=True, capture_output=True, text=True).stdout
    md: dict[str, Any] = json.loads(out)
    return md


def _pkg_name(md: Mapping[str, Any], pkg_id: str) -> str:
    for p in md["packages"]:
        if p["id"] == pkg_id:
            return str(p["name"])
    raise NoticeError(f"cargo metadata: no package {pkg_id}")


def crate_graph(graphs: Iterable[tuple[str, Mapping[str, Any]]]) -> list[Crate]:
    """The crates the normal edges of each (root, metadata) reach, workspace
    members dropped, sorted by name and version. Fails on a source other than
    crates.io, and on a workspace member that is neither a root, reached from
    one, nor host-only."""
    crates: dict[tuple[str, str], Crate] = {}
    members: set[str] = set()
    reached: set[str] = set()
    roots: set[str] = set()
    problems: list[str] = []
    for root_name, md in graphs:
        roots.add(root_name)
        pkgs = {p["id"]: p for p in md["packages"]}
        ws = set(md["workspace_members"])
        members.update(pkgs[i]["name"] for i in ws if i in pkgs)
        nodes = {n["id"]: n for n in md["resolve"]["nodes"]}
        start = [i for i in ws if pkgs[i]["name"] == root_name]
        if not start:
            raise NoticeError(f"cargo metadata: {root_name} is not a workspace member")
        seen = set(start)
        todo = list(start)
        while todo:
            node = nodes[todo.pop()]
            for dep in node["deps"]:
                if not any(k.get("kind") is None for k in dep["dep_kinds"]):
                    continue
                if dep["pkg"] not in seen:
                    seen.add(dep["pkg"])
                    todo.append(dep["pkg"])
        for i in seen:
            p = pkgs[i]
            if i in ws:
                reached.add(p["name"])
                continue
            if p.get("source") not in CRATES_IO:
                problems.append(f"crate {p['name']} {p['version']}: source {p.get('source')} "
                                "is not crates.io (deny.toml [sources])")
                continue
            crates[(p["name"], p["version"])] = Crate(
                p["name"], p["version"], p.get("license") or "", p["source"],
                p["manifest_path"], p.get("license_file"))
    for m in sorted(members - roots - reached - HOST_ONLY):
        problems.append(f"workspace member {m} is neither a shipped root, reachable from one, "
                        "nor in gen_notices.py's HOST_ONLY: classify it")
    if problems:
        raise NoticeError("\n".join(problems))
    return [crates[k] for k in sorted(crates)]


def crate_license_files(crate: Crate, root: Path = ROOT) -> list[tuple[str, str]]:
    """(file name, text) of each license file of `crate`: its `license_file`
    and its top-level LICENSE-like files, else `third_party/crates/<name>-<version>/`."""
    pkg = Path(crate.manifest_path).parent
    names: set[str] = set()
    if crate.license_file:
        lf = pkg / crate.license_file
        if lf.is_file():
            names.add(lf.relative_to(pkg).as_posix() if lf.is_relative_to(pkg) else lf.name)
    if pkg.is_dir():
        for f in pkg.iterdir():
            if f.is_file() and f.name.lower().startswith(LICENSE_PREFIXES):
                names.add(f.name)
    base = pkg
    if not names:
        base = root / CRATES_TP / f"{crate.name}-{crate.version}"
        if base.is_dir():
            names = {f.name for f in base.iterdir() if f.is_file()}
    if not names:
        raise NoticeError(f"crate {crate.name} {crate.version} ships no license file: add its "
                          f"text under {CRATES_TP}/{crate.name}-{crate.version}/")
    return [(n, (base / n).read_text(encoding="utf-8", errors="replace")) for n in sorted(names)]


# ---- Rust ------------------------------------------------------------------

class _Text(html.parser.HTMLParser):
    BLOCK = frozenset({"p", "div", "br", "li", "ul", "ol", "h1", "h2", "h3", "h4", "h5", "h6",
                       "pre", "tr", "table", "hr", "title", "section", "details", "summary"})

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.parts: list[str] = []
        self.skip = 0

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag in ("style", "script"):
            self.skip += 1
        if tag in self.BLOCK:
            self.parts.append("\n")

    def handle_endtag(self, tag: str) -> None:
        if tag in ("style", "script") and self.skip:
            self.skip -= 1
        if tag in self.BLOCK:
            self.parts.append("\n")

    def handle_data(self, data: str) -> None:
        if not self.skip:
            self.parts.append(data)


def html_to_text(doc: str) -> str:
    """Block tags become newlines, entities are unescaped, and runs of blank
    lines collapse to one."""
    p = _Text()
    p.feed(doc)
    p.close()
    lines = [" ".join(line.split()) for line in "".join(p.parts).splitlines()]
    out: list[str] = []
    for line in lines:
        if line or (out and out[-1]):
            out.append(line)
    while out and not out[-1]:
        out.pop()
    return "\n".join(out) + "\n"


def rust_license_ids(doc: str) -> list[str]:
    """The SPDX ids the `License:` lines of the notice's in-tree section name."""
    start = doc.find('id="in-tree-files"')
    end = doc.find('id="out-of-tree-dependencies"')
    if start < 0:
        raise NoticeError("COPYRIGHT-library.html has no in-tree section")
    section = doc[start:end if end > start else len(doc)]
    ids: set[str] = set()
    for m in re.finditer(r"<b>\s*License:\s*</b>\s*([^<]*)", section):
        for tok in re.findall(r"[A-Za-z0-9.+-]+", html.unescape(m.group(1))):
            if tok.upper() not in SPDX_OPERATORS:
                ids.add(tok)
    if not ids:
        raise NoticeError("COPYRIGHT-library.html's in-tree section names no license")
    return sorted(ids)


def rustc_version(vv: str) -> tuple[str, str]:
    """(release, commit-hash) from `rustc -vV`."""
    fields = dict(line.split(": ", 1) for line in vv.splitlines() if ": " in line)
    if "release" not in fields or "commit-hash" not in fields:
        raise NoticeError("rustc -vV names no release or commit-hash")
    return fields["release"].strip(), fields["commit-hash"].strip()


def rust_notice(sysroot: Path, vv: str) -> tuple[str, str, list[tuple[str, str]]]:
    """(marker line, notice text, [(licenses/<ID>.txt, text)]) for the
    standard library of the toolchain at `sysroot`."""
    doc_dir = sysroot / "share" / "doc" / "rust"
    page = doc_dir / "COPYRIGHT-library.html"
    if not page.is_file():
        raise NoticeError("the toolchain has no share/doc/rust/COPYRIGHT-library.html "
                          "(the rustc component installs it)")
    doc = page.read_text(encoding="utf-8", errors="replace")
    release, commit = rustc_version(vv)
    texts = []
    for i in rust_license_ids(doc):
        f = doc_dir / "licenses" / f"{i}.txt"
        if not f.is_file():
            raise NoticeError(f"Rust's notice names {i}, but the toolchain has no licenses/{i}.txt")
        texts.append((f"licenses/{i}.txt", f.read_text(encoding="utf-8", errors="replace")))
    return f"== Rust standard library: rustc {release} ({commit}) ==", html_to_text(doc), texts


# ---- output ----------------------------------------------------------------

@dataclass
class Notices:
    """What the file holds, in order, as (marker line, [(file label, text)])."""

    sections: list[tuple[str, list[tuple[str | None, str]]]]

    def markers(self) -> list[str]:
        return [m for m, _ in self.sections]


def _body(text: str) -> str:
    return text.replace("\r\n", "\n").rstrip() + "\n"


def render(n: Notices) -> str:
    out = ["THIRD-PARTY-NOTICES for vibeOS (ROADMAP §10.9, DESIGN §1.5).\n",
           "Generated by scripts/gen_notices.py; each part starts at a line of the form",
           "`== <part> ==` or `-- <entry> --`.\n"]
    for marker, files in n.sections:
        out.append(marker)
        for label, text in files:
            if label is not None:
                out.append(f"[{label}]")
            out.append(_body(text))
    return "\n".join(out).rstrip() + "\n"


def missing(text: str, expected: Iterable[str]) -> list[str]:
    """The marker lines of `expected` that are not lines of `text`."""
    lines = set(text.splitlines())
    return [m for m in expected if m not in lines]


def collect(root: Path, arch: str, limine_dir: Path | None,
            graphs: list[tuple[str, Mapping[str, Any]]], sysroot: Path, vv: str) -> Notices:
    problems = check_limine(root, limine_dir)
    if problems:
        raise NoticeError("\n".join(problems))
    sections: list[tuple[str, list[tuple[str | None, str]]]] = []
    sections.append(("== vibeOS ==", [(None, (root / "LICENSE").read_text(encoding="utf-8"))]))
    tp = root / LIMINE_TP
    man = limine_manifest(tp)
    sections.append((f"== Limine {man['release']} (binary {man['binary_commit']}) ==",
                     [(None, (tp / "LICENSE").read_text(encoding="utf-8"))]))
    for p in man["project"]:
        sections.append((f"-- Limine third-party: {p['name']} ({p['license']}) --",
                         [(f, (tp / f).read_text(encoding="utf-8", errors="replace"))
                          for f in p["files"]]))
    sections.append((f"== Rust crates ({arch}) ==", []))
    errors = []
    for c in crate_graph(graphs):
        try:
            files: list[tuple[str | None, str]] = list(crate_license_files(c, root))
        except NoticeError as e:
            errors.append(str(e))
            continue
        sections.append((f"-- crate: {c.name} {c.version} ({c.license}) --", files))
    if errors:
        raise NoticeError("\n".join(errors))
    marker, text, lic = rust_notice(sysroot, vv)
    sections.append((marker, [(None, text)]))
    for name, t in lic:
        sections.append((f"-- rust {name} --", [(None, t)]))
    sections.append(("== Adapted in-tree files ==", []))
    headers, problems = check_provenance.scan(root)
    if problems:
        raise NoticeError("\n".join(problems))
    for h in sorted(headers, key=lambda h: h.path):
        sections.append((f"-- in-tree: {h.path} from {h.url} {h.upstream_path} @ {h.rev} "
                         f"({h.license}) --", [(None, h.notice)]))
    return Notices(sections)


def expected_markers(root: Path, n: Notices) -> list[str]:
    """The markers the file must hold: the parts, every 3RDPARTY.md entry,
    and Rust's notice, whatever the sections were built from."""
    tp = root / LIMINE_TP
    man = limine_manifest(tp)
    by_name = {p["name"]: p for p in man["project"]}
    want = ["== vibeOS ==", f"== Limine {man['release']} (binary {man['binary_commit']}) =="]
    for name, _ in parse_3rdparty((tp / "3RDPARTY.md").read_text(encoding="utf-8")):
        lic = by_name.get(name, {}).get("license", "?")
        want.append(f"-- Limine third-party: {name} ({lic}) --")
    want += [m for m in n.markers() if m.startswith(("== Rust crates", "-- crate: "))]
    want += [m for m in n.markers() if m.startswith("== Rust standard library: rustc ")] or [
        "== Rust standard library: rustc"]
    want.append("== Adapted in-tree files ==")
    return want


def toolchain(root: Path) -> tuple[Path, str]:
    """(sysroot, `rustc -vV`) of the toolchain rust-toolchain.toml pins."""
    def rustc(*args: str) -> str:
        return subprocess.run(["rustc", *args], cwd=root, check=True, capture_output=True,
                              text=True).stdout
    return Path(rustc("--print", "sysroot").strip()), rustc("-vV")


def generate(root: Path, arch: str, limine_dir: Path | None) -> str:
    if arch not in ROOTS:
        raise NoticeError(f"no shipped roots for arch {arch}")
    graphs: list[tuple[str, Mapping[str, Any]]] = []
    members: set[str] | None = None
    for name, triple in ROOTS[arch]:
        md = cargo_metadata(root, triple)
        if members is None:
            members = {p["name"] for p in md["packages"] if p["id"] in md["workspace_members"]}
        if name in members:
            graphs.append((name, md))
    sysroot, vv = toolchain(root)
    n = collect(root, arch, limine_dir, graphs, sysroot, vv)
    text = render(n)
    absent = missing(text, expected_markers(root, n))
    if absent:
        raise NoticeError("missing from the notices: " + ", ".join(absent))
    return text


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="gen_notices.py")
    ap.add_argument("--out", required=True, type=Path, help="the file to write")
    ap.add_argument("--arch", default="x86_64", choices=sorted(ROOTS))
    ap.add_argument("--limine-dir", type=Path, default=None,
                    help="the Limine clone, whose LICENSE must equal third_party/limine/LICENSE")
    args = ap.parse_args(argv if argv is not None else [])
    try:
        text = generate(ROOT, args.arch, args.limine_dir)
    except (NoticeError, subprocess.CalledProcessError, OSError) as e:
        detail = getattr(e, "stderr", None)
        print(f"gen_notices: {e}" + (f"\n{detail}" if detail else ""), file=sys.stderr)
        return 1
    out: Path = args.out
    out.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=out.parent, prefix=f".{out.name}.")
    with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    os.replace(tmp, out)
    os.chmod(out, 0o644)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
