"""Host tests for scripts/changelog_section.py (B3 release notes) and
scripts/check_changelog.py (DOC4)."""

from __future__ import annotations

import contextlib
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import check_changelog

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
        self.assertIn("## [0.8.0]\n", proc.stdout)
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


def valid_changelog(entry_lines: int) -> str:
    """A changelog that passes every rule but the length one, whose one entry spans
    `entry_lines` lines."""
    entry = "- first line\n" + "".join(f"  more {i}\n" for i in range(entry_lines - 1))
    needles = "\n".join(check_changelog.STYLE_NEEDLES)
    return (
        f"# Changelog\n\n{needles}\n\n## [Unreleased]\n\n### Added\n\n{entry}\n"
        "## [0.8.0] - 2026-09-19\n\n- Phase 8 exit.\n\n"
        "[0.8.0]: https://github.com/devinreuschel/vibeOS/releases/tag/v0.8.0\n"
    )


def run_check(text: str) -> tuple[int, str]:
    """Run check_changelog.main() on `text`; return its exit code and stderr."""
    with tempfile.TemporaryDirectory() as d:
        root = Path(d)
        path = root / "CHANGELOG.md"
        path.write_text(text, encoding="utf-8")
        err = io.StringIO()
        with (
            mock.patch.object(check_changelog, "ROOT", root),
            mock.patch.object(check_changelog, "CHANGELOG", path),
            contextlib.redirect_stderr(err),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            rc = check_changelog.main()
        return rc, err.getvalue()


class TestCheckChangelog(unittest.TestCase):
    def run_main(self, text: str) -> tuple[int, str]:
        return run_check(text)

    def test_entry_lengths_counts_continuations(self) -> None:
        text = "- one\n  two\n  three\n- four\n\n- five\n  six\nprose\n"
        self.assertEqual(check_changelog.entry_lengths(text), [(1, 3), (4, 1), (6, 2)])

    def test_entry_at_max_passes(self) -> None:
        rc, err = self.run_main(valid_changelog(check_changelog.MAX_LINES))
        self.assertEqual(rc, 0, err)

    def test_entry_over_max_fails(self) -> None:
        n = check_changelog.MAX_LINES + 1
        rc, err = self.run_main(valid_changelog(n))
        self.assertEqual(rc, 1)
        self.assertIn("CHANGELOG.md:", err)
        self.assertIn(f"entry is {n} lines (max {check_changelog.MAX_LINES})", err)

    def test_missing_style_needle_fails(self) -> None:
        needle = check_changelog.STYLE_NEEDLES[0]
        text = valid_changelog(1).replace(needle, "")
        rc, err = self.run_main(text)
        self.assertEqual(rc, 1)
        self.assertIn(f"missing style rule: {needle!r}", err)

    def test_real_changelog_passes(self) -> None:
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(check_changelog.main(), 0, err.getvalue())


class TestEntryLength(unittest.TestCase):
    """DOC4: a changelog entry is at most 2 lines (ROADMAP §10.1)."""

    def test_max_lines_is_two(self) -> None:
        self.assertEqual(check_changelog.MAX_LINES, 2)

    def test_three_line_entry_fails(self) -> None:
        rc, err = run_check(valid_changelog(3))
        self.assertEqual(rc, 1)
        self.assertIn("entry is 3 lines (max 2)", err)

    def test_two_line_entry_passes(self) -> None:
        rc, err = run_check(valid_changelog(2))
        self.assertEqual(rc, 0, err)


if __name__ == "__main__":
    unittest.main()
