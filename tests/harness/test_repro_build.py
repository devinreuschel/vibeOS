"""Host tests for scripts/repro_build.py (ROADMAP §10.2, F151, F152)."""

from __future__ import annotations

import contextlib
import io
import os
import tempfile
import unittest
from pathlib import Path

from scripts import repro_build
from scripts.repro_build import (
    BuildSpec,
    build_env,
    compare,
    find_host_paths,
    first_difference,
    made_paths,
    needles_for,
    outputs,
    plan,
    scan,
)

FILES = ("build/kernels/vibeos-default.elf", "build/vibeos.iso", "build/initrd.fat")


def tree(root: Path, files: dict[str, bytes]) -> Path:
    for rel, data in files.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
    return root


class PlanTest(unittest.TestCase):
    def test_two_builds_apart(self) -> None:
        a, b = plan(Path("/w"), share_rustup=False)
        self.assertNotEqual(len(str(a.checkout)), len(str(b.checkout)))
        self.assertNotEqual(a.cargo_home, b.cargo_home)
        self.assertNotEqual(a.rustup_home, b.rustup_home)
        for s in (a, b):
            for p in (s.checkout, s.cargo_home, s.rustup_home):
                self.assertTrue(str(p).startswith("/w/"), p)

    def test_share_rustup(self) -> None:
        a, b = plan(Path("/w"), share_rustup=True)
        self.assertEqual(a.rustup_home, b.rustup_home)
        self.assertEqual(a.rustup_home, repro_build.default_rustup_home())
        self.assertNotEqual(a.cargo_home, b.cargo_home)
        self.assertNotIn(a.rustup_home, made_paths(a))

    def test_made_paths(self) -> None:
        a, _ = plan(Path("/w"), share_rustup=False)
        self.assertEqual(made_paths(a), [a.checkout.parent, a.cargo_home, a.rustup_home])


class OutputsTest(unittest.TestCase):
    def test_lists_elfs_isos_and_initrd(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = tree(Path(d), {**{f: b"x" for f in FILES},
                                  "build/kernels/vibeos-default.ksyms.rs": b"",
                                  "build/vibeos.iso.xorriso-version": b"",
                                  "build/results/x.json": b""})
            self.assertEqual(outputs(root), sorted(FILES))

    def test_empty_fails(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(FileNotFoundError):
                outputs(Path(d))


class CompareTest(unittest.TestCase):
    def test_first_difference(self) -> None:
        self.assertIsNone(first_difference(b"abc", b"abc"))
        self.assertEqual(first_difference(b"abc", b"abd"), 2)
        self.assertEqual(first_difference(b"ab", b"abc"), 2)
        self.assertEqual(first_difference(b"", b"a"), 0)

    def test_identical(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            files = {f: f.encode() for f in FILES}
            a = tree(Path(d) / "a", files)
            b = tree(Path(d) / "b", files)
            self.assertEqual(compare(a, b), [])

    def test_missing_extra_and_differing(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            a = tree(Path(d) / "a", {FILES[0]: b"same", FILES[1]: b"0123", FILES[2]: b"i"})
            b = tree(Path(d) / "b", {FILES[0]: b"same", FILES[1]: b"01x3",
                                     "build/vibeos-ktest.iso": b"k"})
            errors = compare(a, b)
            self.assertIn(f"build/initrd.fat: only in {a}", errors)
            self.assertIn(f"build/vibeos-ktest.iso: only in {b}", errors)
            self.assertIn("build/vibeos.iso: differs at offset 0x2", errors)
            self.assertEqual(len(errors), 3)


class HostPathTest(unittest.TestCase):
    def test_find_host_paths(self) -> None:
        data = b"\x00/home/u/src/vibeOS/src/main.rs\x00"
        self.assertEqual(find_host_paths(data, ["/home/u/src/vibeOS", "/opt/x", ""]),
                         ["/home/u/src/vibeOS"])
        self.assertEqual(find_host_paths(b"clean", ["/home/u"]), [])

    def test_needles_cover_both_builds(self) -> None:
        a, b = plan(Path("/w"), share_rustup=False)
        needles = needles_for([a, b])
        for s in (a, b):
            for p in (s.checkout, s.cargo_home, s.rustup_home):
                self.assertIn(str(p), needles)
        self.assertIn(str(Path.home()), needles)
        self.assertNotIn("/", needles)

    def test_scan(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            root = tree(Path(d), {FILES[0]: b"x/w/a/vibeOS/src\x00", FILES[1]: b"clean"})
            self.assertEqual(scan(root, ["/w/a/vibeOS"]),
                             [f"{FILES[0]}: holds host path /w/a/vibeOS"])
            self.assertEqual(scan(root, ["/nowhere"]), [])


class EnvTest(unittest.TestCase):
    def test_build_env(self) -> None:
        spec = BuildSpec(Path("/w/a/vibeOS"), Path("/w/cargo-a"), Path("/w/rustup-a"))
        old = os.environ.get("CARGO_TARGET_DIR")
        os.environ["CARGO_TARGET_DIR"] = "/shared/target"
        try:
            env = build_env(spec)
        finally:
            if old is None:
                del os.environ["CARGO_TARGET_DIR"]
            else:
                os.environ["CARGO_TARGET_DIR"] = old
        self.assertNotIn("CARGO_TARGET_DIR", env)
        self.assertEqual(env["CARGO_HOME"], "/w/cargo-a")
        self.assertEqual(env["RUSTUP_HOME"], "/w/rustup-a")


class MainTest(unittest.TestCase):
    def test_refuses_a_used_workdir(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "a").mkdir()
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                rc = repro_build.main(["--workdir", d])
            self.assertEqual(rc, 2)
            self.assertIn("already exists", err.getvalue())
            self.assertTrue((Path(d) / "a").is_dir())


if __name__ == "__main__":
    unittest.main()
