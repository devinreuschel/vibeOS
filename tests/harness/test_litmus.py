"""Host tests for scripts/check_litmus.py (ROADMAP §11.7)."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts import check_litmus
from scripts.check_litmus import check_helpers, litmus_files


class Tree:
    def __init__(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        self.write(
            "src/arch/aarch64/mod.rs",
            "/// tests/litmus/seqlock.litmus\nfn dma_wmb() {}\n"
            "/// tests/litmus/log_ring.litmus\nfn dma_rmb() {}\n"
            "/// tests/litmus/dma_mb.litmus\nfn dma_mb() {}\n"
            "/// Arm ARM B2.3.5: Device vs Normal, herd7 does not model.\n"
            "unsafe fn mmio_read<T>() {}\n"
            "/// Arm ARM B2.3.5: Device vs Normal, herd7 does not model.\n"
            "unsafe fn mmio_write<T>() {}\n",
        )
        self.write("tests/litmus/seqlock.litmus", "AArch64 seqlock\n")
        self.write("tests/litmus/log_ring.litmus", "AArch64 log\n")
        self.write("tests/litmus/dma_mb.litmus", "AArch64 dma\n")

    def close(self) -> None:
        self._dir.cleanup()

    def write(self, rel: str, text: str) -> None:
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")


class LitmusTest(unittest.TestCase):
    def setUp(self) -> None:
        self.t = Tree()
        self.addCleanup(self.t.close)

    def test_named_tests_pass(self) -> None:
        self.assertEqual(check_helpers(self.t.root), [])

    def test_missing_comment_fails(self) -> None:
        self.t.write("src/arch/aarch64/mod.rs", "fn dma_wmb() {}\n")
        errs = check_helpers(self.t.root)
        self.assertTrue(any("dma_wmb" in e and "names no" in e for e in errs), errs)

    def test_missing_file_fails(self) -> None:
        self.t.write(
            "src/arch/aarch64/mod.rs",
            "/// tests/litmus/gone.litmus\nfn dma_wmb() {}\n",
        )
        errs = check_helpers(self.t.root)
        self.assertTrue(any("gone.litmus" in e for e in errs), errs)

    def test_arm_arm_citation_passes_without_file(self) -> None:
        self.t.write(
            "src/arch/aarch64/mod.rs",
            "/// Arm ARM G8.2: TLB maintenance, herd7 cannot express.\nfn dma_mb() {}\n",
        )
        self.assertEqual(check_helpers(self.t.root), [])

    def test_litmus_files_lists_tree(self) -> None:
        names = [p.name for p in litmus_files(self.t.root)]
        self.assertEqual(sorted(names), ["dma_mb.litmus", "log_ring.litmus", "seqlock.litmus"])


class TestRepo(unittest.TestCase):
    def test_repo_helpers_pass(self) -> None:
        self.assertEqual(check_helpers(), [])

    def test_usage(self) -> None:
        self.assertEqual(check_litmus.main(["--nope"]), 2)


class ObservationTest(unittest.TestCase):
    def test_both_models_use_the_same_rule(self) -> None:
        err = check_litmus.observation_error
        self.assertIsNone(err("tests/litmus/seqlock.litmus", "aarch64.cat", "Never"))
        self.assertIsNotNone(err("tests/litmus/seqlock.litmus", "aarch64.cat", "Sometimes"))
        self.assertIsNone(err("tests/litmus/seqlock_x86.litmus", "x86tso.cat", "Never"))
        self.assertIsNotNone(err("tests/litmus/seqlock_x86.litmus", "x86tso.cat", "Sometimes"))
        self.assertIsNone(err("tests/litmus/dma_mb_relaxed.litmus", "aarch64.cat", "Sometimes"))
        self.assertIsNone(err("tests/litmus/dma_mb_x86_relaxed.litmus", "x86tso.cat", "Always"))
        self.assertIsNotNone(err("tests/litmus/dma_mb_x86_relaxed.litmus", "x86tso.cat", "Never"))

    def test_run_herd7_reads_x86_verdicts(self) -> None:
        t = Tree()
        self.addCleanup(t.close)
        t.write("tests/litmus/mp_x86.litmus", "X86 mp\n")

        def fake_run(_cmd: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(_cmd, 0, "Observation mp Sometimes 1 0\n", "")

        with (
            patch("scripts.check_litmus.which", return_value="/usr/bin/herd7"),
            patch("scripts.check_litmus.subprocess.run", side_effect=fake_run),
        ):
            errs = check_litmus.run_herd7(t.root)
        self.assertTrue(any("mp_x86.litmus" in e and "x86tso.cat" in e for e in errs), errs)


if __name__ == "__main__":
    unittest.main()
