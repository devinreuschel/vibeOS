"""Unit tests for the e2e harness itself (DESIGN §8.3).

Runs under `python3 -m unittest discover`. Standard-library only.
"""

from __future__ import annotations

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import (  # noqa: E402
    HarnessError,
    Marker,
    check_markers_in_order,
    contains_panic,
)


class TestOrderedMarkerCheck(unittest.TestCase):
    def test_all_present_in_order(self) -> None:
        lines = [
            "vibeOS: serial online",
            "vibeOS: limine: rev 3 ok",
            "vibeOS: boot: phase0 done",
        ]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker("vibeOS: limine: rev 3 ok", "b"),
            Marker("vibeOS: boot: phase0 done", "c"),
        ]
        result = check_markers_in_order(lines, markers)
        self.assertEqual(result.matched, ["a", "b", "c"])

    def test_out_of_order_fails(self) -> None:
        lines = [
            "vibeOS: boot: phase0 done",  # too early
            "vibeOS: serial online",
            "vibeOS: limine: rev 3 ok",
        ]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker("vibeOS: limine: rev 3 ok", "b"),
            Marker("vibeOS: boot: phase0 done", "c"),
        ]
        with self.assertRaises(HarnessError):
            check_markers_in_order(lines, markers)

    def test_missing_final_marker_fails(self) -> None:
        lines = ["vibeOS: serial online", "vibeOS: limine: rev 3 ok"]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker("vibeOS: limine: rev 3 ok", "b"),
            Marker("vibeOS: boot: phase0 done", "c"),
        ]
        with self.assertRaises(HarnessError) as cm:
            check_markers_in_order(lines, markers)
        # The error names the missing marker's `name`, not its substring.
        self.assertIn("'c'", str(cm.exception))

    def test_panic_signature_fails_fast(self) -> None:
        lines = [
            "vibeOS: serial online",
            "panicked at src/foo.rs:1:1",
            "vibeOS: boot: phase0 done",
        ]
        markers = [Marker("vibeOS: boot: phase0 done", "c")]
        with self.assertRaises(HarnessError) as cm:
            check_markers_in_order(lines, markers)
        self.assertIn("panicked at", str(cm.exception))

    def test_extra_lines_between_markers_are_fine(self) -> None:
        lines = [
            "chatter",
            "vibeOS: serial online",
            "more chatter",
            "vibeOS: limine: rev 3 ok",
            "even more",
            "vibeOS: boot: phase0 done",
        ]
        markers = [
            Marker("vibeOS: serial online", "a"),
            Marker("vibeOS: limine: rev 3 ok", "b"),
            Marker("vibeOS: boot: phase0 done", "c"),
        ]
        check_markers_in_order(lines, markers)


class TestPanicSignatureScan(unittest.TestCase):
    def test_matches_exception_mnemonic(self) -> None:
        self.assertTrue(contains_panic("cpu halted on #PF at ..."))
        self.assertTrue(contains_panic("panicked at src/main.rs:12:5"))

    def test_english_prose_is_not_a_false_positive(self) -> None:
        # DESIGN §9.7: matching prose is a footgun. `page fault` must NOT
        # trigger the scanner; only `#PF` does.
        self.assertFalse(contains_panic("shell help: 'demo a page fault'"))
        self.assertFalse(contains_panic("help text about general protection"))

    def test_double_fault_phrase_matches_intentionally(self) -> None:
        # The literal phrase 'double fault' IS listed in PANIC_SIGNATURES.
        # If someone puts it in help text later, they need to rename the
        # help text, not the harness.
        self.assertTrue(contains_panic("we hit a double fault"))


if __name__ == "__main__":
    unittest.main()
