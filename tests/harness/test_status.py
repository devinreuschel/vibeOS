"""Host tests for scripts/check_status.py (box status notes, ROADMAP §10.9)."""

from __future__ import annotations

import contextlib
import io
import unittest

from scripts import check_status
from scripts.check_status import check_status as check
from scripts.gatelib import parse_boxes


def errs(body: str) -> list[str]:
    return check(parse_boxes(f"## Phase 10: P\n\n### 10.1 S\n\n{body}\n"))


class TestStatus(unittest.TestCase):
    def test_ticked_notes_fail(self) -> None:
        for note in ("lands in §10.5", "deferred to §11.2", "Reopened by F001; lands in §10.4"):
            with self.subTest(note=note):
                got = errs(f"- [x] a box; {note}.")
                self.assertEqual(len(got), 1)
                self.assertIn("ticked box holds", got[0])

    def test_ticked_notes_in_code_span_pass(self) -> None:
        body = "- [x] fails on `lands in §`, `deferred to §`, and ``Reopened by`` notes"
        self.assertEqual(errs(body), [])

    def test_prose_lands_in_passes(self) -> None:
        self.assertEqual(errs("- [x] the fix lands in the allocator"), [])

    def test_open_note_passes(self) -> None:
        self.assertEqual(errs("- [ ] a box. Reopened by F001; lands in §10.5."), [])

    def test_two_phase_note_fails(self) -> None:
        for note in ("§10.2 and §11.1", "§10.2, §10.4, and §12.1", "§10.2 or §11", "§9.1, §10.2"):
            with self.subTest(note=note):
                got = errs(f"- [ ] a box; lands in {note}.")
                self.assertEqual(len(got), 1)
                self.assertIn("names Phases", got[0])

    def test_one_phase_note_passes(self) -> None:
        for note in ("§10.2 and §10.11", "§10.4, and its proof", "§10.2, §10.7 or §10"):
            with self.subTest(note=note):
                self.assertEqual(errs(f"- [ ] a box; lands in {note}."), [])

    def test_funded_lines_are_ignored(self) -> None:
        text = ("## Phase 10: P\n\n- [ ] ok\n\n# Funded goals\n\n### G\n\n"
                "- [x] a goal line that lands in §10.2\n- [ ] lands in §10.2 and §11.1\n")
        self.assertEqual(check(parse_boxes(text)), [])

    def test_real_tree_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_status.main([]), 0)
        self.assertEqual(out.getvalue(), "check_status: ok\n")


if __name__ == "__main__":
    unittest.main()
