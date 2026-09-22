"""Host tests for scripts/changelog_section.py (B3 release notes)."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "changelog_section.py"
SAMPLE = """# Changelog

## [Unreleased]

### Added

- next thing

## [0.8.0] - 2026-09-19

### Added

- Phase 8 exit.

[Unreleased]: https://example.test/compare/v0.8.0...HEAD
[0.8.0]: https://example.test/releases/tag/v0.8.0
"""


def run_script(*args: str, input_text: str | None = None) -> subprocess.CompletedProcess[str]:
    cmd = [sys.executable, str(SCRIPT), *args]
    if input_text is None:
        return subprocess.run(cmd, cwd=ROOT, text=True, capture_output=True, check=False)
    with tempfile.NamedTemporaryFile("w", suffix=".md", encoding="utf-8", delete=False) as fh:
        fh.write(input_text)
        path = fh.name
    try:
        return subprocess.run(
            [*cmd, "--file", path],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
    finally:
        Path(path).unlink(missing_ok=True)


class TestChangelogSection(unittest.TestCase):
    def test_extracts_0_8_0_from_repo(self) -> None:
        proc = run_script("0.8.0")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("## [0.8.0] - 2026-09-19", proc.stdout)
        self.assertIn("Phase 8", proc.stdout)
        self.assertNotIn("## [Unreleased]", proc.stdout)
        self.assertNotIn("paused", proc.stdout.lower())
        self.assertNotIn("[0.8.0]: https://", proc.stdout)

    def test_tag_strips_v_prefix(self) -> None:
        proc = run_script("--tag", "v0.8.0", input_text=SAMPLE)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(
            proc.stdout,
            "## [0.8.0] - 2026-09-19\n\n### Added\n\n- Phase 8 exit.\n",
        )

    def test_missing_section_fails(self) -> None:
        proc = run_script("9.9.9", input_text=SAMPLE)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("no changelog section ## [9.9.9]", proc.stderr)


if __name__ == "__main__":
    unittest.main()
