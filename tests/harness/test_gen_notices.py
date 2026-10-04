"""Host tests for scripts/gen_notices.py (ROADMAP §10.9, DESIGN §1.5).

Fixtures are built in temporary directories: a repository with setup.sh's
pins, LICENSE and `third_party/limine/`, a registry of crate sources, cargo
metadata naming them, and a toolchain sysroot with Rust's notice.
"""

from __future__ import annotations

import os
import tempfile
import unittest
from collections.abc import Mapping
from pathlib import Path
from typing import Any

from scripts import gen_notices
from scripts.gen_notices import Crate, NoticeError, Notices
from tests.harness.gitfixture import TempRepo

COPYRIGHT = "Copy" + "right"
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
COMMIT = "c82c3708b3304be806b2492dc2ce34e219c6f989"
VV = ("rustc 1.2.3-nightly (abc 2026-01-01)\nbinary: rustc\ncommit-hash: abcdef\n"
      "host: x86_64-unknown-linux-gnu\nrelease: 1.2.3-nightly\n")

THIRDPARTY = """# 3rd Party Software Acknowledgments

- [pdgzip](https://github.com/iczelia/pdgzip) (0BSD) is used for gzip
decompression.

- [Flanterm](https://example.invalid/Flanterm)
(BSD-2-Clause) is used for text.
    - an indented sub-item
"""

MANIFEST = f"""release = "v12.9.1"
commit = "{COMMIT}"

[[project]]
name = "pdgzip"
license = "0BSD"
files = ["pdgzip/LICENSE"]

[[project]]
name = "Flanterm"
license = "BSD-2-Clause"
files = ["flanterm/LICENSE"]
"""

RUST_HTML = """<!DOCTYPE html><html><body>
<h1>Copyright notices for The Rust Standard Library</h1>
<p>Dual-licensed &amp; kept.</p>
<h2 id="in-tree-files">In-tree files</h2>
<div><p><b>License:</b> Apache-2.0 OR MIT</p></div>
<div><p><b>License:</b> Apache-2.0 WITH LLVM-exception AND (Apache-2.0 OR MIT)</p></div>
<h2 id="out-of-tree-dependencies">Out-of-tree dependencies</h2>
<p><b>License:</b> Zlib</p>
</body></html>
"""


def write(base: Path, files: Mapping[str, str | bytes]) -> None:
    for rel, text in files.items():
        p = base / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(text, bytes):
            p.write_bytes(text)
        else:
            p.write_text(text, encoding="utf-8")


class Fixture:
    """A repository, a registry and a sysroot in one temporary directory."""

    def __init__(self) -> None:
        self.repo = TempRepo()
        self.root = self.repo.path
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.registry = self.tmp / "home" / ".cargo" / "registry" / "src" / "index"
        self.sysroot = self.tmp / "home" / ".rustup" / "toolchains" / "t"
        self.repo.commit("t", {
            "setup.sh": ('LIMINE_TAG="${LIMINE_TAG:-v12.9.1}"\n'
                         f'LIMINE_COMMIT="${{LIMINE_COMMIT:-{COMMIT}}}"\n'),
            "LICENSE": f"MIT License\n\n{COPYRIGHT} (c) vibeOS\n",
            "src/main.rs": "fn main() {}\n",
            "third_party/limine/LICENSE": f"{COPYRIGHT} Limine\n",
            "third_party/limine/3RDPARTY.md": THIRDPARTY,
            "third_party/limine/MANIFEST.toml": MANIFEST,
            "third_party/limine/pdgzip/LICENSE": "0bsd text\n",
            "third_party/limine/flanterm/LICENSE": "bsd text\n",
        })
        write(self.sysroot / "share/doc/rust", {
            "COPYRIGHT-library.html": RUST_HTML,
            "licenses/Apache-2.0.txt": "apache text\n",
            "licenses/MIT.txt": "mit text\n",
            "licenses/LLVM-exception.txt": "llvm exception text\n",
            "licenses/Zlib.txt": "zlib, out of tree\n",
        })
        self.packages: list[dict[str, Any]] = []
        self.nodes: dict[str, list[dict[str, Any]]] = {}
        self.members: list[str] = []

    def cleanup(self) -> None:
        self.repo.cleanup()
        self._tmp.cleanup()

    def member(self, name: str) -> str:
        pid = f"path+file:///ws/{name}#0.1.0"
        self.packages.append({"id": pid, "name": name, "version": "0.1.0", "license": "MIT",
                              "source": None, "manifest_path": f"/ws/{name}/Cargo.toml",
                              "license_file": None})
        self.nodes[pid] = []
        self.members.append(pid)
        return pid

    def crate(self, name: str, version: str = "1.0.0", files: Mapping[str, str] | None = None,
              source: str = CRATES_IO, license_file: str | None = None) -> str:
        pid = f"{source}#{name}@{version}"
        d = self.registry / f"{name}-{version}"
        write(d, {"Cargo.toml": "[package]\n", "src/lib.rs": "",
                  **({"LICENSE-MIT": f"{name} mit\n"} if files is None else files)})
        self.packages.append({"id": pid, "name": name, "version": version,
                              "license": "MIT OR Apache-2.0", "source": source,
                              "manifest_path": str(d / "Cargo.toml"), "license_file": license_file})
        self.nodes[pid] = []
        return pid

    def dep(self, frm: str, to: str, *kinds: str | None) -> None:
        self.nodes[frm].append({"pkg": to, "dep_kinds": [
            {"kind": k, "target": None} for k in (kinds or (None,))]})

    def metadata(self) -> dict[str, Any]:
        return {"packages": self.packages, "workspace_members": self.members,
                "resolve": {"nodes": [{"id": i, "deps": d} for i, d in self.nodes.items()]}}

    def standard(self) -> None:
        """vibeos -> vibeos-core -> dep; hostlib-tests host-only."""
        k = self.member("vibeos")
        c = self.member("vibeos-core")
        h = self.member("vibeos-hostlib-tests")
        d = self.crate("dep")
        self.dep(k, c)
        self.dep(c, d)
        self.dep(h, c)

    def collect(self) -> Notices:
        return gen_notices.collect(self.root, "x86_64", None, [("vibeos", self.metadata())],
                                   self.sysroot, VV)

    def text(self) -> str:
        return gen_notices.render(self.collect())


class FixtureCase(unittest.TestCase):
    def setUp(self) -> None:
        self.f = Fixture()
        self.addCleanup(self.f.cleanup)


class TestGraph(FixtureCase):
    def test_graph_normal_edges_only(self) -> None:
        f = self.f
        k = f.member("vibeos")
        normal = f.crate("normal")
        dev = f.crate("devonly")
        build = f.crate("buildonly")
        pm = f.crate("procmacro")
        pm_dep = f.crate("procmacro-dep")
        both = f.crate("both")
        under_dev = f.crate("under-dev")
        f.dep(k, normal)
        f.dep(k, dev, "dev")
        f.dep(k, build, "build")
        f.dep(normal, pm)
        f.dep(pm, pm_dep)
        f.dep(k, both, "dev", None)
        f.dep(dev, under_dev)
        got = [c.name for c in gen_notices.crate_graph([("vibeos", f.metadata())])]
        self.assertEqual(got, ["both", "normal", "procmacro", "procmacro-dep"])

    def test_workspace_members_excluded(self) -> None:
        self.f.standard()
        got = gen_notices.crate_graph([("vibeos", self.f.metadata())])
        self.assertEqual([(c.name, c.version) for c in got], [("dep", "1.0.0")])

    def test_unclassified_member_fails(self) -> None:
        self.f.standard()
        self.f.member("vibeos-new")
        with self.assertRaisesRegex(NoticeError, "workspace member vibeos-new"):
            gen_notices.crate_graph([("vibeos", self.f.metadata())])

    def test_non_crates_io_source_fails(self) -> None:
        f = self.f
        k = f.member("vibeos")
        f.dep(k, f.crate("forked", source="git+https://example.invalid/forked#abc"))
        with self.assertRaisesRegex(NoticeError, "forked 1.0.0: source git\\+https"):
            gen_notices.crate_graph([("vibeos", f.metadata())])


class TestLicenseFiles(FixtureCase):
    def crate(self, pid: str) -> Crate:
        p = next(p for p in self.f.packages if p["id"] == pid)
        return Crate(p["name"], p["version"], p["license"], p["source"], p["manifest_path"],
                     p["license_file"])

    def test_license_files_found(self) -> None:
        pid = self.f.crate("x", files={"LICENSE-APACHE": "a", "copying": "c", "NOTICE": "n",
                                       "docs/LICENSE": "nested", "README.md": "r",
                                       "legal.txt": "named"},
                           license_file="legal.txt")
        got = gen_notices.crate_license_files(self.crate(pid), self.f.root)
        self.assertEqual([n for n, _ in got], ["LICENSE-APACHE", "NOTICE", "copying", "legal.txt"])

    def test_crate_without_license_fails(self) -> None:
        pid = self.f.crate("bare", files={})
        with self.assertRaisesRegex(NoticeError, "crate bare 1.0.0 ships no license file"):
            gen_notices.crate_license_files(self.crate(pid), self.f.root)
        k = self.f.member("vibeos")
        self.f.dep(k, pid)
        with self.assertRaisesRegex(NoticeError, "bare"):
            self.f.collect()

    def test_crate_license_from_third_party_crates(self) -> None:
        pid = self.f.crate("bare", files={})
        write(self.f.root, {"third_party/crates/bare-1.0.0/LICENSE-MIT": "kept text\n"})
        got = gen_notices.crate_license_files(self.crate(pid), self.f.root)
        self.assertEqual(got, [("LICENSE-MIT", "kept text\n")])


class TestMissing(FixtureCase):
    def test_missing_crate_reported(self) -> None:
        self.f.standard()
        n = self.f.collect()
        want = gen_notices.expected_markers(self.f.root, n)
        self.assertIn("-- crate: dep 1.0.0 (MIT OR Apache-2.0) --", want)
        self.assertEqual(gen_notices.missing(gen_notices.render(n), want), [])
        cut = Notices([s for s in n.sections if not s[0].startswith("-- crate: dep")])
        self.assertEqual(gen_notices.missing(gen_notices.render(cut), want),
                         ["-- crate: dep 1.0.0 (MIT OR Apache-2.0) --"])

    def test_missing_3rdparty_entry_reported(self) -> None:
        self.f.standard()
        n = self.f.collect()
        want = gen_notices.expected_markers(self.f.root, n)
        cut = Notices([s for s in n.sections if "Flanterm" not in s[0]])
        self.assertEqual(gen_notices.missing(gen_notices.render(cut), want),
                         ["-- Limine third-party: Flanterm (BSD-2-Clause) --"])

    def test_missing_rust_notice_reported(self) -> None:
        self.f.standard()
        n = self.f.collect()
        want = gen_notices.expected_markers(self.f.root, n)
        cut = Notices([s for s in n.sections if not s[0].startswith("== Rust standard")])
        self.assertEqual(gen_notices.missing(gen_notices.render(cut), want),
                         ["== Rust standard library: rustc 1.2.3-nightly (abcdef) =="])
        (self.f.sysroot / "share/doc/rust/COPYRIGHT-library.html").unlink()
        with self.assertRaisesRegex(NoticeError, "COPYRIGHT-library.html"):
            self.f.collect()


class TestLimine(FixtureCase):
    def test_real_fixture_passes(self) -> None:
        self.assertEqual(gen_notices.check_limine(self.f.root), [])
        self.assertEqual(gen_notices.parse_3rdparty(THIRDPARTY),
                         [("pdgzip", "0BSD"), ("Flanterm", "BSD-2-Clause")])

    def test_limine_release_mismatch_fails(self) -> None:
        write(self.f.root, {"third_party/limine/MANIFEST.toml":
                            MANIFEST.replace('"v12.9.1"', '"v12.9.0"')})
        probs = gen_notices.check_limine(self.f.root)
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("release 'v12.9.0', but setup.sh's LIMINE_TAG is v12.9.1", probs[0])

    def test_limine_pins_read_from_defaults_not_environment(self) -> None:
        old = os.environ.get("LIMINE_TAG")
        os.environ["LIMINE_TAG"] = "v1.0.0"
        try:
            self.assertEqual(gen_notices.check_limine(self.f.root), [])
        finally:
            if old is None:
                del os.environ["LIMINE_TAG"]
            else:
                os.environ["LIMINE_TAG"] = old

    def test_limine_commit_mismatch_fails(self) -> None:
        write(self.f.root, {"third_party/limine/MANIFEST.toml": MANIFEST.replace(COMMIT, "0" * 40)})
        probs = gen_notices.check_limine(self.f.root)
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("commit '0000", probs[0])

    def test_limine_license_differs_from_unpacked_fails(self) -> None:
        unpacked = self.f.tmp / "limine"
        write(unpacked, {"LICENSE": f"{COPYRIGHT} Limine\n"})
        self.assertEqual(gen_notices.check_limine(self.f.root, unpacked), [])
        write(unpacked, {"LICENSE": f"{COPYRIGHT} Limine, newer\n"})
        probs = gen_notices.check_limine(self.f.root, unpacked)
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("differs from the unpacked Limine's LICENSE", probs[0])
        # A missing directory (the check job runs no setup.sh) is no problem.
        self.assertEqual(gen_notices.check_limine(self.f.root, self.f.tmp / "none"), [])

    def test_3rdparty_entry_without_manifest_project_fails(self) -> None:
        extra = "\n- [stb_image](https://x.invalid) (MIT) loads images.\n"
        write(self.f.root, {"third_party/limine/3RDPARTY.md": THIRDPARTY + extra})
        probs = gen_notices.check_limine(self.f.root)
        self.assertEqual(probs, ["third_party/limine/MANIFEST.toml: no [[project]] for "
                                 "3RDPARTY.md's 'stb_image'"])
        with self.assertRaisesRegex(NoticeError, "stb_image"):
            self.f.standard()
            self.f.collect()

    def test_project_file_missing_fails(self) -> None:
        (self.f.root / "third_party/limine/pdgzip/LICENSE").unlink()
        probs = gen_notices.check_limine(self.f.root)
        self.assertEqual(len(probs), 1, probs)
        self.assertIn("pdgzip/LICENSE: missing", probs[0])


class TestOutput(FixtureCase):
    def test_rust_licenses_named_ids_included(self) -> None:
        self.f.standard()
        text = self.f.text()
        for i in ("Apache-2.0", "LLVM-exception", "MIT"):
            self.assertIn(f"-- rust licenses/{i}.txt --", text.splitlines())
        self.assertNotIn("-- rust licenses/Zlib.txt --", text)
        self.assertIn("\nDual-licensed & kept.\n", text)
        (self.f.sysroot / "share/doc/rust/licenses/LLVM-exception.txt").unlink()
        with self.assertRaisesRegex(NoticeError, "no licenses/LLVM-exception.txt"):
            self.f.collect()

    def test_provenance_notice_included(self) -> None:
        self.f.standard()
        notice = f"{COPYRIGHT} (c) 2020 Someone\n\nPermission to use, copy ..."
        rev = "0123456789abcdef0123456789abcdef01234567"
        text = (f"// Provenance: https://example.invalid/u.git src/u.c @ {rev}\n"
                "// Upstream-License: ISC\n"
                + "".join(f"// {t}".rstrip() + "\n" for t in notice.splitlines())
                + "fn f() {}\n")
        self.f.repo.commit("t", {"src/adapted.rs": text})
        out = self.f.text().splitlines()
        marker = (f"-- in-tree: src/adapted.rs from https://example.invalid/u.git src/u.c @ {rev} "
                  "(ISC) --")
        i = out.index(marker)
        self.assertEqual(out[i + 1:i + 4], notice.splitlines())

    def test_no_host_paths(self) -> None:
        self.f.standard()
        text = self.f.text()
        for p in (str(self.f.tmp), str(self.f.root), "/home", ".cargo/registry", ".rustup"):
            self.assertNotIn(p, text)
        self.assertIn("[LICENSE-MIT]\ndep mit\n", text)

    def test_deterministic(self) -> None:
        self.f.standard()
        self.f.member("vibeos-user")
        a = self.f.metadata()
        second = self.f.crate("second")
        self.f.dep(self.f.members[-1], second)
        b = self.f.metadata()
        one = gen_notices.render(gen_notices.collect(
            self.f.root, "x86_64", None, [("vibeos", a), ("vibeos-user", b)], self.f.sysroot, VV))
        self.f.packages.reverse()
        two = gen_notices.render(gen_notices.collect(
            self.f.root, "x86_64", None, [("vibeos-user", b), ("vibeos", a)], self.f.sysroot, VV))
        self.assertEqual(one, two)
        markers = [m for m in one.splitlines() if m.startswith("-- crate: ")]
        self.assertEqual(markers, ["-- crate: dep 1.0.0 (MIT OR Apache-2.0) --",
                                   "-- crate: second 1.0.0 (MIT OR Apache-2.0) --"])


class TestRealTree(unittest.TestCase):
    def test_gen_notices_real_tree(self) -> None:
        root = gen_notices.ROOT
        self.assertEqual(gen_notices.check_limine(root, root / "limine"), [])
        text = gen_notices.generate(root, "x86_64", root / "limine")
        n_markers = [m for m in text.splitlines() if m.startswith(("== ", "-- "))]
        self.assertIn("-- crate: limine 0.6.5 (MIT OR Apache-2.0) --", n_markers)
        self.assertTrue(any(m.startswith("== Rust standard library: rustc ") for m in n_markers))
        listed = gen_notices.parse_3rdparty(
            (root / "third_party/limine/3RDPARTY.md").read_text(encoding="utf-8"))
        self.assertEqual(len(listed), 8, listed)
        for name, _ in listed:
            marker = f"-- Limine third-party: {name} ("
            self.assertTrue(any(m.startswith(marker) for m in n_markers), name)
        self.assertNotIn(str(Path.home()), text)
        for p in (".cargo/registry", ".rustup"):
            self.assertNotIn(p, text)


if __name__ == "__main__":
    unittest.main()
