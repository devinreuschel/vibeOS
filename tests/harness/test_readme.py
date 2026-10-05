"""Host tests for scripts/check_readme.py (ROADMAP Phase 11 exit)."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts.check_readme import check, makefile_targets, sections

README_OK = """# vibeOS

## x86_64 quickstart

    ./setup.sh
    make check
    make
    make run
    make test

## aarch64 quickstart

On an Apple Silicon Mac, HVF:

    ./setup.sh
    make ARCH=aarch64
    VIBEOS_QEMU_ACCEL=hvf make ARCH=aarch64 run
"""

MAKEFILE = """
check:
	true
run:
	true
test:
	true
all:
	true
"""


class Tree:
    def __init__(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        self.write("README.md", README_OK)
        self.write("Makefile", MAKEFILE)

    def close(self) -> None:
        self._dir.cleanup()

    def write(self, rel: str, text: str) -> None:
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")


class ReadmeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.t = Tree()
        self.addCleanup(self.t.close)

    def test_good_tree_passes(self) -> None:
        self.assertEqual(check(self.t.root), [])

    def test_missing_x86_section_fails(self) -> None:
        self.t.write("README.md", README_OK.replace("## x86_64 quickstart", "## Intel quickstart"))
        errs = check(self.t.root)
        self.assertTrue(any("x86_64 quickstart" in e for e in errs), errs)

    def test_missing_aarch64_section_fails(self) -> None:
        self.t.write("README.md", README_OK.replace("## aarch64 quickstart", "## arm quickstart"))
        errs = check(self.t.root)
        self.assertTrue(any("aarch64 quickstart" in e for e in errs), errs)

    def test_aarch64_needs_make_arch_run_and_hvf(self) -> None:
        self.t.write(
            "README.md",
            README_OK.replace("On an Apple Silicon Mac, HVF:", "On an Apple Silicon Mac:").replace(
                "VIBEOS_QEMU_ACCEL=hvf make ARCH=aarch64 run",
                "make ARCH=aarch64 iso",
            ),
        )
        errs = check(self.t.root)
        self.assertTrue(any("make ARCH=aarch64 run" in e for e in errs), errs)
        self.assertTrue(any("HVF" in e for e in errs), errs)

    def test_unknown_make_target_fails(self) -> None:
        self.t.write("README.md", README_OK + "\n    make nope\n")
        # nope is after the aarch64 section end... append inside aarch64 by rewrite
        self.t.write(
            "README.md",
            README_OK.replace("VIBEOS_QEMU_ACCEL=hvf make ARCH=aarch64 run",
                              "VIBEOS_QEMU_ACCEL=hvf make ARCH=aarch64 run\n    make nope"),
        )
        errs = check(self.t.root)
        self.assertTrue(any("make nope" in e for e in errs), errs)

    def test_helpers(self) -> None:
        self.assertIn("check", makefile_targets(MAKEFILE))
        titles = [t for t, _ in sections(README_OK)]
        self.assertEqual(titles, ["x86_64 quickstart", "aarch64 quickstart"])


class TestRepo(unittest.TestCase):
    def test_repo_passes(self) -> None:
        self.assertEqual(check(), [])


if __name__ == "__main__":
    unittest.main()
