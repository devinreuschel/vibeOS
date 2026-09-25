"""Host tests for scripts/check_review_refs.py (kernel review traceability)."""

from __future__ import annotations

import unittest

from scripts.check_review_refs import check, parse_review, parse_roadmap

REVIEW = """### Critical

#### F001 · first

**Severity:** CRITICAL · **Confidence:** Confirmed

#### F002 · second

**Severity:** HIGH · LATENT (COW) · **Confidence:** Likely

#### F003 · third

**Severity:** LOW · **Confidence:** Suspected
"""


class TestParse(unittest.TestCase):
    def test_review_severity_and_latent(self) -> None:
        f = parse_review(REVIEW)
        self.assertEqual(sorted(f), ["F001", "F002", "F003"])
        self.assertEqual(f["F001"].severity, "CRITICAL")
        self.assertFalse(f["F001"].latent)
        self.assertTrue(f["F002"].latent)
        self.assertEqual(f["F003"].severity, "LOW")

    def test_roadmap_box_states(self) -> None:
        cites = parse_roadmap("- [x] done (F001)\n- [ ] open (F002, F003)\nprose F001\n")
        states = [(c.line, c.fid, c.box) for c in cites]
        self.assertTrue(all(c.phase10 for c in cites))
        self.assertEqual(
            states,
            [(1, "F001", "closed"), (2, "F002", "open"), (2, "F003", "open"), (3, "F001", None)],
        )


class TestCheck(unittest.TestCase):
    def setUp(self) -> None:
        self.findings = parse_review(REVIEW)

    def test_all_cited_passes(self) -> None:
        cites = parse_roadmap("- [ ] a (F001)\n- [ ] b (F002)\n- [x] c (F003)\n")
        self.assertEqual(check(self.findings, cites, closed=False), [])

    def test_missing_finding_fails(self) -> None:
        cites = parse_roadmap("- [ ] a (F001, F002)\n")
        self.assertEqual(check(self.findings, cites, closed=False),
                         ["F003: not cited in docs/ROADMAP.md"])

    def test_unknown_id_fails(self) -> None:
        cites = parse_roadmap("- [ ] a (F001, F002, F003, F099)\n")
        errs = check(self.findings, cites, closed=False)
        self.assertEqual(errs, ["ROADMAP.md:1: F099 is not a finding in KERNEL_REVIEW.md"])

    def test_closed_mode_flags_open_critical_only(self) -> None:
        # F001 is CRITICAL and not LATENT: an open box fails. F002 is LATENT and
        # F003 is LOW: open boxes citing them pass.
        cites = parse_roadmap("- [ ] a (F001)\n- [ ] b (F002)\n- [ ] c (F003)\n")
        self.assertEqual(check(self.findings, cites, closed=False), [])
        self.assertEqual(check(self.findings, cites, closed=True),
                         ["ROADMAP.md:1: F001 (CRITICAL) cited by an open box"])

    def test_closed_mode_ignores_phase_11_on(self) -> None:
        # A later phase may cite a live finding as a cross-reference.
        cites = parse_roadmap("- [x] a (F001)\n## Phase 11: Portability\n- [ ] b (F001)\n"
                              "- [ ] c (F002, F003)\n")
        self.assertFalse(cites[1].phase10)
        self.assertEqual(check(self.findings, cites, closed=True), [])

    def test_closed_mode_passes_when_boxes_closed(self) -> None:
        cites = parse_roadmap("- [x] a (F001)\n- [ ] b (F002)\n- [ ] c (F003)\n")
        self.assertEqual(check(self.findings, cites, closed=True), [])


if __name__ == "__main__":
    unittest.main()
