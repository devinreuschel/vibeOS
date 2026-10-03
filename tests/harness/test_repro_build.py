"""Host tests for scripts/repro_build.py (ROADMAP §10.2, F151, F152)."""

from __future__ import annotations

import contextlib
import io
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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
        self.assertEqual(env["VIBEOS_SKIP_MSRV"], "1")  # setup.sh: no MSRV toolchain


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


class WorkdirTest(unittest.TestCase):
    def test_runner_temp_when_a_directory(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            with mock.patch.dict(os.environ, {"RUNNER_TEMP": d}):
                self.assertEqual(repro_build.workdir_parent(), Path(d))
            with mock.patch.dict(os.environ, {"RUNNER_TEMP": str(Path(d) / "none")}):
                self.assertIsNone(repro_build.workdir_parent())
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("RUNNER_TEMP", None)
            self.assertIsNone(repro_build.workdir_parent())


def fake_build(spec: BuildSpec, commit: str) -> None:
    """What `build` leaves for `spec`: outputs, a target dir, and both homes."""
    tree(spec.checkout, {f: commit.encode() for f in FILES})
    tree(spec.checkout, {"target/x86_64-unknown-none/debug/big": b"x"})
    tree(spec.cargo_home, {"registry/cache/c": b"c"})
    tree(spec.rustup_home, {"toolchains/nightly/bin/rustc": b"r"})


class PruneTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.w = Path(self._tmp.name)
        # never the real ~/.rustup, whatever prune does
        patcher = mock.patch.object(repro_build, "default_rustup_home",
                                    return_value=self.w / "caller-rustup")
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_prune_keeps_outputs(self) -> None:
        a, _ = plan(self.w, share_rustup=False)
        fake_build(a, "c")
        gone = repro_build.prune(a)
        self.assertEqual(gone, [a.checkout / "target", a.cargo_home, a.rustup_home])
        for p in gone:
            self.assertFalse(p.exists(), p)
        self.assertEqual(outputs(a.checkout), sorted(FILES))
        self.assertEqual(repro_build.prune(a), [])  # nothing left to delete

    def test_prune_keeps_a_shared_rustup(self) -> None:
        a, _ = plan(self.w, share_rustup=True)
        fake_build(a, "c")
        self.assertEqual(repro_build.prune(a), [a.checkout / "target", a.cargo_home])
        self.assertTrue((self.w / "caller-rustup" / "toolchains").is_dir())


class BuildStepsTest(unittest.TestCase):
    def steps(self, share_rustup: bool) -> list[tuple[list[str], dict[str, str] | None]]:
        calls: list[tuple[list[str], dict[str, str] | None]] = []
        with tempfile.TemporaryDirectory() as d:
            spec, _ = plan(Path(d), share_rustup)
            with (
                mock.patch.object(repro_build, "run",
                                  side_effect=lambda cmd, cwd, env=None: calls.append((cmd, env))),
                mock.patch("shutil.copytree"),
                mock.patch.object(repro_build, "default_rustup_home",
                                  return_value=spec.rustup_home if share_rustup
                                  else Path(d) / "caller-rustup"),
            ):
                repro_build.build(spec, "c" * 40)
        return calls

    def test_own_rustup_gets_a_minimal_toolchain_and_no_msrv(self) -> None:
        cmds = [c for c, _ in self.steps(share_rustup=False)]
        self.assertEqual(cmds[2:], [
            ["rustup", "toolchain", "install", "--profile", "minimal", "--no-self-update"],
            ["./setup.sh"],
            ["make", "isos"],
        ])
        for cmd, env in self.steps(share_rustup=False)[2:]:
            assert env is not None
            self.assertEqual(env["VIBEOS_SKIP_MSRV"], "1", cmd)

    def test_shared_rustup_installs_nothing(self) -> None:
        cmds = [c for c, _ in self.steps(share_rustup=True)]
        self.assertEqual(cmds[2:], [["./setup.sh"], ["make", "isos"]])


class PruneOrderTest(unittest.TestCase):
    """`main` with a fake build: what of build A is on disk when B starts."""

    def run_main(self, *extra: str) -> tuple[int, dict[str, bool], BuildSpec]:
        d = Path(self.enterContext(tempfile.TemporaryDirectory()))
        a, _ = plan(d, share_rustup=False)
        at_b: dict[str, bool] = {}

        def recording_build(spec: BuildSpec, commit: str) -> None:
            if spec.checkout != a.checkout:
                at_b.update({
                    "target": (a.checkout / "target").exists(),
                    "cargo_home": a.cargo_home.exists(),
                    "rustup_home": a.rustup_home.exists(),
                    "outputs": all((a.checkout / f).is_file() for f in FILES),
                })
            fake_build(spec, commit)

        with (
            mock.patch.object(repro_build, "resolve", return_value="c" * 40),
            mock.patch.object(repro_build, "build", side_effect=recording_build),
            mock.patch.object(repro_build, "default_rustup_home",
                              return_value=d / "caller-rustup"),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            rc = repro_build.main(["--workdir", str(d), *extra])
        return rc, at_b, a

    def test_build_b_starts_after_a_is_pruned(self) -> None:
        rc, at_b, a = self.run_main()
        self.assertEqual(rc, 0)
        self.assertEqual(at_b, {"target": False, "cargo_home": False, "rustup_home": False,
                                "outputs": True})
        self.assertFalse(a.checkout.parent.exists())  # both builds removed afterwards

    def test_keep_leaves_build_a_whole(self) -> None:
        rc, at_b, a = self.run_main("--keep")
        self.assertEqual(rc, 0)
        self.assertEqual(at_b, {"target": True, "cargo_home": True, "rustup_home": True,
                                "outputs": True})
        self.assertTrue((a.checkout / "target").is_dir())


if __name__ == "__main__":
    unittest.main()
