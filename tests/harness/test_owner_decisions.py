"""Host tests for scripts/check_owner_decisions.py (owner decision records, ROADMAP §10.9)."""

from __future__ import annotations

import contextlib
import io
import unittest

from scripts import check_owner_decisions
from scripts.check_owner_decisions import check_owner_decisions as check
from scripts.check_owner_decisions import parse_records
from scripts.gatelib import parse_boxes

ASK_HERE = "an agent writes an OWNER DECISION block here stating each format"
ASK_DESIGN = "an agent writes an OWNER DECISION block in DESIGN §2.10 that states it"
LINK = "[design review H007](reviews/DESIGN_REVIEWS.md)"

INVARIANTS = "# 2. Invariants\n\n### 2.10 Trust boundaries\n\ntext\n\n{record}\n\n### 2.11 Next\n"


def roadmap(box: str, record: str = "", later: str = "") -> str:
    return (f"# Roadmap\n\n## Phase 36: P\n\n### 36.6 Media\n\n{box}\n\n{record}\n\n"
            f"### 36.7 Next\n\n{later}\n")


def errs(road: str, invariants: str = "", extra: dict[str, str] | None = None) -> list[str]:
    files = {"docs/ROADMAP.md": road,
             "docs/INVARIANTS.md": invariants or INVARIANTS.format(record=""),
             **(extra or {})}
    return check(parse_boxes(road), files)


def record(section: str = "36.6", kind: str = "decision", day: str = "2026-09-23") -> str:
    return (f"**Codecs (owner {kind}, {day}, ROADMAP §{section}): option (b).** The owner\n"
            "chose it.")


class TestOwnerDecisions(unittest.TestCase):
    def test_ticked_here_box_without_record_fails(self) -> None:
        got = errs(roadmap(f"- [x] {ASK_HERE}"))
        self.assertEqual(len(got), 1)
        self.assertIn("holds no dated owner record naming ROADMAP §36.6", got[0])

    def test_ticked_here_box_with_record_passes(self) -> None:
        self.assertEqual(errs(roadmap(f"- [x] {ASK_HERE}", record())), [])

    def test_record_for_another_section_fails(self) -> None:
        for section in ("36.7", "36.60", "3.6"):
            with self.subTest(section=section):
                self.assertEqual(len(errs(roadmap(f"- [x] {ASK_HERE}", record(section)))), 1)

    def test_record_outside_the_place_fails(self) -> None:
        got = errs(roadmap(f"- [x] {ASK_HERE}", later=record()))
        self.assertEqual(len(got), 1)

    def test_design_topic_file_place(self) -> None:
        road = roadmap(f"- [x] {ASK_DESIGN}")
        self.assertEqual(len(errs(road)), 1)
        inv = INVARIANTS.format(record=record())
        self.assertEqual(errs(road, inv), [])

    def test_open_box_passes_without_record(self) -> None:
        self.assertEqual(errs(roadmap(f"- [ ] {ASK_HERE}")), [])
        self.assertEqual(errs(roadmap(f"- [ ] {ASK_DESIGN}")), [])

    def test_owner_position_passes(self) -> None:
        self.assertEqual(errs(roadmap(f"- [x] {ASK_HERE}", record(kind="position"))), [])

    def test_bad_date_fails(self) -> None:
        got = errs(roadmap(f"- [x] {ASK_HERE}", record(day="2026-02-30")))
        self.assertEqual(len(got), 2)
        self.assertIn("bad date", got[0])

    def test_unresolvable_place_fails(self) -> None:
        for box in ("- [ ] writes an OWNER DECISION block in DESIGN §2.99 that",
                    "- [ ] writes an OWNER DECISION block in VIBEFS §3.1 that"):
            with self.subTest(box=box):
                got = errs(roadmap(box))
                self.assertEqual(len(got), 1)
                self.assertIn("names no heading", got[0])

    def test_asks_for_wording_is_not_asking(self) -> None:
        box = ("- [x] `scripts/check_owner_decisions.py` fails on a ticked box that asks for an "
               "OWNER DECISION block unless ... `writes an OWNER DECISION block here`")
        self.assertEqual(errs(roadmap(box)), [])

    def test_answered_needed_block_fails(self) -> None:
        block = "> **OWNER DECISION NEEDED (review H007)**: which key custody."
        rec = f"**Key custody (owner decision, 2026-09-23, {LINK}): option (b).** Text."
        got = errs(roadmap("- [ ] other", f"{rec}\n\n{block}"))
        self.assertEqual(len(got), 1)
        self.assertIn("OWNER DECISION NEEDED (review H007) is answered", got[0])

    def test_unanswered_needed_block_passes(self) -> None:
        block = "> **OWNER DECISION NEEDED (review J002)**: agents act as you."
        rec = f"**Key custody (owner decision, 2026-09-23, {LINK}): option (b).** Text."
        self.assertEqual(errs(roadmap("- [ ] other", f"{rec}\n\n{block}")), [])

    def test_record_with_link_parses(self) -> None:
        text = ("intro\n\n**Interim posture (owner decision, 2026-09-23, [design review\n"
                "G006](reviews/DESIGN_REVIEWS.md)).** The owner accepted it.\n")
        e: list[str] = []
        got = parse_records("docs/X.md", text, e)
        self.assertEqual(e, [])
        self.assertEqual([(r.line, r.kind, r.date) for r in got], [(3, "decision", "2026-09-23")])
        self.assertIn("design review G006", got[0].body)
        self.assertTrue(got[0].body.endswith("(reviews/DESIGN_REVIEWS.md)"))

    def test_real_tree_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_owner_decisions.main([]), 0)
        self.assertEqual(out.getvalue(), "check_owner_decisions: ok\n")


if __name__ == "__main__":
    unittest.main()
