"""Host tests for scripts/check_provenance.py (ROADMAP §10.9, DESIGN §1.5).

Planted strings are built at runtime so that no line of this file matches the
scanner.
"""

from __future__ import annotations

import contextlib
import io
import tempfile
import textwrap
import unittest
from pathlib import Path

from scripts import check_provenance
from scripts.check_provenance import Header, fetch_check, parse_header, scan
from tests.harness.gitfixture import TempRepo

COPYRIGHT = "Copy" + "right"
SPDX = "SPDX-" + "License-Identifier"
URL = "https://example.invalid/upstream.git"
REV = "0123456789abcdef0123456789abcdef01234567"
MIT_BODY = check_provenance.TEMPLATES["MIT"].replace("<>", "THE AUTHORS OR COPYRIGHT HOLDERS")


def header(lic: str, notice: str | None = None, marker: str = "//") -> str:
    lines = [
        f"Provenance: {URL} src/thing.c @ {REV}",
        f"Upstream-License: {lic}",
    ]
    if notice is None:
        notice = f"{COPYRIGHT} (c) 2020 Someone\n\n" + MIT_BODY.strip()
    lines.extend(notice.splitlines())
    return "".join(f"{marker} {t}".rstrip() + "\n" for t in lines)


def problems_of(files: dict[str, str]) -> list[str]:
    with tempfile.TemporaryDirectory() as t:
        root = Path(t)
        for rel, text in files.items():
            (root / rel).parent.mkdir(parents=True, exist_ok=True)
            (root / rel).write_text(text, encoding="utf-8")
        return scan(root, list(files))[1]


class TestScan(unittest.TestCase):
    def test_copyright_without_header_fails(self) -> None:
        probs = problems_of({"src/a.rs": f"// {COPYRIGHT} 2020 Someone\nfn main() {{}}\n"})
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("src/a.rs:1: a copyright line but no provenance header", probs[0])

    def test_copyright_sign_without_header_fails(self) -> None:
        probs = problems_of({"a.py": "# © 2020 Someone\n"})
        self.assertEqual(len(probs), 1, probs)

    def test_spdx_without_header_fails(self) -> None:
        probs = problems_of({"a.S": f"/* {SPDX}: MIT */\n"})
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("an SPDX license line", probs[0])

    def test_header_gpl_only_fails(self) -> None:
        probs = problems_of({"a.rs": header("GPL-2.0-only")})
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("a.rs:2: Upstream-License 'GPL-2.0-only'", probs[0])

    def test_header_apache_only_fails(self) -> None:
        probs = problems_of({"a.rs": header("Apache-2.0")})
        self.assertEqual(len(probs), 1, probs)

    def test_header_and_expression_fails(self) -> None:
        probs = problems_of({"a.rs": header("MIT AND Apache-2.0")})
        self.assertEqual(len(probs), 1, probs)

    def test_header_missing_field_fails(self) -> None:
        text = f"// Provenance: {URL} src/thing.c @ {REV}\n// {COPYRIGHT} 2020 Someone\n"
        probs = problems_of({"a.rs": text})
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("Upstream-License", probs[0])
        text = f"// Provenance: {URL} src/thing.c\n// Upstream-License: MIT\n// x\n"
        self.assertEqual(len(problems_of({"a.rs": text})), 1)

    def test_header_empty_notice_fails(self) -> None:
        probs = problems_of({"a.rs": header("MIT", notice="") + "\nfn main() {}\n"})
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("no upstream notice", probs[0])

    def test_header_mit_passes(self) -> None:
        text = "//! A module.\n\n" + header("MIT") + "\nfn main() {}\n"
        self.assertEqual(problems_of({"a.rs": text}), [])
        h = parse_header(text, "a.rs")
        assert h is not None
        self.assertEqual((h.line, h.url, h.upstream_path, h.rev, h.license),
                         (3, URL, "src/thing.c", REV, "MIT"))
        self.assertTrue(h.notice.startswith(f"{COPYRIGHT} (c) 2020 Someone"))
        self.assertTrue(h.notice.endswith("OTHER DEALINGS IN THE SOFTWARE."))

    def test_header_dual_gpl_or_mit_passes(self) -> None:
        self.assertEqual(problems_of({"a.py": header("GPL-2.0 OR MIT", marker="#")}), [])
        self.assertEqual(problems_of({"a.asm": header("(GPL-2.0-only OR BSD-3-Clause)",
                                                      marker=";")}), [])

    def test_block_comment_header_passes(self) -> None:
        body = header("ISC", notice=f"{COPYRIGHT} 2019 Someone\n\nPermission to use ...",
                      marker=" *")
        text = "/*\n" + body + " */\n#include <x.h>\n"
        self.assertEqual(problems_of({"a.c": text}), [])
        h = parse_header(text)
        assert h is not None
        self.assertEqual(h.license, "ISC")
        self.assertEqual(h.notice, f"{COPYRIGHT} 2019 Someone\n\nPermission to use ...")

    def test_header_after_line_40_is_no_header(self) -> None:
        text = "\n" * 40 + header("MIT")
        self.assertIsNone(parse_header(text))
        self.assertEqual(len(problems_of({"a.rs": text})), 1)

    def test_third_party_and_license_excluded(self) -> None:
        repo = TempRepo()
        try:
            marked = f"# {COPYRIGHT} 2020 Someone\n"
            repo.commit("t", {
                "third_party/x/y.py": marked,
                "LICENSE": marked,
                "notes.md": marked,
                "data.bin": marked,
                "Makefile": "all:\n",
                "a.rs": "fn main() {}\n",
            })
            self.assertEqual(check_provenance.source_files(repo.path), ["Makefile", "a.rs"])
            self.assertEqual(scan(repo.path), ([], []))
            repo.commit("t", {"Makefile": marked})
            self.assertEqual(len(scan(repo.path)[1]), 1)
        finally:
            repo.cleanup()

    def test_undecodable_file_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as t:
            (Path(t) / "a.rs").write_bytes(b"\xff\xfe" + COPYRIGHT.encode())
            self.assertEqual(scan(Path(t), ["a.rs"]), ([], []))

    def test_notice_only(self) -> None:
        ok = ["MIT", "0BSD", "MIT/Apache-2.0", "Apache-2.0 or MIT", "(MIT AND ISC) OR GPL-3.0"]
        bad = ["Apache-2.0", "GPL-2.0 WITH Linux-syscall-note", "MIT AND GPL-2.0", "", "MIT OR"]
        for e in ok:
            self.assertTrue(check_provenance.notice_only(e), e)
        for e in bad:
            self.assertFalse(check_provenance.notice_only(e), e)

    def test_real_tree_clean(self) -> None:
        headers, problems = scan()
        self.assertEqual(problems, [])
        for h in headers:
            self.assertTrue(check_provenance.notice_only(h.license), h)


def hdr(lic: str) -> Header:
    return Header("a.c", 1, URL, "src/thing.c", REV, lic, f"{COPYRIGHT} x")


def stub(text: str) -> tuple[list[tuple[str, str, str]], check_provenance.Fetch]:
    calls: list[tuple[str, str, str]] = []

    def fetch(url: str, rev: str, path: str) -> str:
        calls.append((url, rev, path))
        return text

    return calls, fetch


class TestFetch(unittest.TestCase):
    def test_fetch_spdx_match_passes(self) -> None:
        calls, fetch = stub(f"/* {SPDX}: (GPL-2.0 or   MIT) */\nint x;\n")
        self.assertEqual(fetch_check([hdr("(GPL-2.0 OR MIT)")], fetch), [])
        self.assertEqual(calls, [(URL, REV, "src/thing.c")])

    def test_fetch_spdx_mismatch_fails(self) -> None:
        _, fetch = stub(f"// {SPDX}: GPL-2.0-only\n")
        probs = fetch_check([hdr("GPL-2.0-only OR MIT")], fetch)
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("states 'GPL-2.0-only'", probs[0])

    def test_fetch_no_spdx_text_match_passes(self) -> None:
        body = textwrap.fill(" ".join(MIT_BODY.split()), width=53).splitlines()
        half = len(body) // 2
        text = ("/*\n * " + COPYRIGHT + " (c) 2020 Someone\n *\n"
                + "".join(f" * {t}\n" for t in body[:half]) + " */\n")
        # The second half as `//` lines, which are a second block: fails.
        split = text + "".join(f"// {t}\n" for t in body[half:])
        _, fetch = stub(split)
        self.assertEqual(len(fetch_check([hdr("MIT")], fetch)), 1)
        # All of it reflowed in one `/* */` block with ` * ` prefixes: passes.
        text = ("/*\n * " + COPYRIGHT + " (c) 2020 Someone\n *\n"
                + "".join(f" * {t}\n" for t in body) + " */\nint x;\n")
        _, fetch = stub(text)
        self.assertEqual(fetch_check([hdr("MIT")], fetch), [])
        # And as `//` lines.
        _, fetch = stub("".join(f"//  {t}\n" for t in body) + "int x;\n")
        self.assertEqual(fetch_check([hdr("GPL-2.0 OR MIT")], fetch), [])

    def test_fetch_no_spdx_text_mismatch_fails(self) -> None:
        body = MIT_BODY.replace("free of charge", "for a fee")
        _, fetch = stub("".join(f"# {t}\n" for t in body.splitlines()))
        probs = fetch_check([hdr("MIT")], fetch)
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("does not hold the text of 'MIT'", probs[0])

    def test_fetch_no_template_fails(self) -> None:
        _, fetch = stub("/* Permission to use, copy, modify ... */\n")
        probs = fetch_check([hdr("HPND")], fetch)
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("no license text for HPND", probs[0])

    def test_fetch_no_headers_fetches_nothing(self) -> None:
        calls, fetch = stub("")
        self.assertEqual(fetch_check([], fetch), [])
        self.assertEqual(calls, [])
        repo = TempRepo()
        try:
            repo.commit("t", {"a.rs": "fn main() {}\n"})
            with contextlib.redirect_stdout(io.StringIO()) as out:
                rc = check_provenance.main(["--fetch"], root=repo.path, fetch=fetch)
            self.assertEqual(rc, 0)
            self.assertIn("0 fetched", out.getvalue())
        finally:
            repo.cleanup()
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
