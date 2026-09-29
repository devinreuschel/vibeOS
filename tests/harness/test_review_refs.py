"""Host tests for scripts/check_review_refs.py (kernel review traceability)."""

from __future__ import annotations

import contextlib
import io
import unittest
from unittest import mock

from scripts import check_review_refs
from scripts.check_review_refs import (
    DEPARTURES,
    Finding,
    check,
    check_design_reviews,
    check_needs,
    check_placement,
    design_review_inputs,
    doc_headings,
    parse_review,
    parse_roadmap,
    tag_phase,
    wave,
)
from scripts.gatelib import REVIEW as REVIEW_PATH
from scripts.gatelib import NeedRow, NeedsFile, parse_boxes, roadmap_boxes

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


NEEDS_ROADMAP = """## Phase 9: Users
- [x] nine done
## Phase 10: Consolidation
- [ ] ten open (F001)
- [ ] ten needed
- [ ] ten lands after the box above
- [x] ten closed partner
- [ ] ten twice needed
- [ ] ten wanted from outside
prose: ten twice
## Phase 11: Portability
- [ ] eleven lands with nothing
# Beyond
- [ ] beyond line
"""


def row(key: str, needs: tuple[str, ...] = (), closes: tuple[str, ...] = ()) -> NeedRow:
    return NeedRow(key, needs, closes, "")


class TestNeeds(unittest.TestCase):
    """The needs-file rules (ROADMAP §10.9, the check_review_refs.py needs box)."""

    LANDS_ROW = row("lands after", ("ten needed",))

    def errs(self, rows10: list[NeedRow], rows11: list[NeedRow] | None = None,
             roots: list[str] | None = None) -> list[str]:
        needs: list[NeedsFile] = [(10, [*rows10, self.LANDS_ROW], roots or [])]
        if rows11 is not None:
            needs.append((11, rows11, []))
        return check_needs(NEEDS_ROADMAP, needs)

    def test_clean_rows_pass(self) -> None:
        self.assertEqual(self.errs([row("ten open", ("nine done",))], roots=["ten open"]), [])

    def test_key_matches_no_line(self) -> None:
        for rows, roots in (([row("absent")], []), ([row("ten open", ("absent",))], []),
                            ([row("ten open", (), ("absent",))], []), ([], ["absent"])):
            with self.subTest(rows=rows, roots=roots):
                errs = self.errs(rows, roots=roots)
                self.assertEqual(len(errs), 1)
                self.assertIn("'absent' matches no line", errs[0])

    def test_key_matches_several_lines(self) -> None:
        for rows, roots in (([row("ten ")], []), ([row("ten open", ("ten ",))], []),
                            ([row("ten open", (), ("ten ",))], []), ([], ["ten "])):
            with self.subTest(rows=rows, roots=roots):
                errs = self.errs(rows, roots=roots)
                self.assertEqual(len(errs), 1)
                self.assertIn("matches 7 lines", errs[0])

    def test_key_matches_a_line_that_is_not_a_box(self) -> None:
        for rows, roots in (([row("prose: ten")], []), ([row("ten open", ("prose",))], []),
                            ([row("ten open", (), ("prose",))], []), ([], ["prose"])):
            with self.subTest(rows=rows, roots=roots):
                errs = self.errs(rows, roots=roots)
                self.assertEqual(len(errs), 1)
                self.assertIn("which is not a box", errs[0])

    def test_later_phase_need(self) -> None:
        errs = self.errs([row("ten open", ("eleven lands",))])
        self.assertEqual(len(errs), 1)
        self.assertIn("needs L12, a box in Phase 11, after Phase 10", errs[0])
        # Phase 11's own file may need a Phase 11 box.
        self.assertEqual(self.errs([], [row("eleven lands", ("ten open",)),
                                        row("ten needed", ("eleven lands",))]), [])
        errs = self.errs([row("ten open", ("beyond line",))])
        self.assertEqual(len(errs), 1)
        self.assertIn("a box in no phase", errs[0])
        # A `closes` entry is not a need.
        self.assertEqual(self.errs([row("ten open", (), ("eleven lands",))]), [])

    def test_lands_clause_needs_a_row(self) -> None:
        errs = check_needs(NEEDS_ROADMAP, [(10, [], [])])
        self.assertEqual(errs, ["ROADMAP.md:6: a lands clause and no needs row: "
                                "ten lands after the box above"])
        # As a row's key, a `needs` entry, or a `closes` entry.
        for r in (row("lands after"), row("ten open", ("lands after",)),
                  row("ten open", (), ("lands after",))):
            with self.subTest(row=r):
                self.assertEqual(check_needs(NEEDS_ROADMAP, [(10, [r], [])]), [])

    def test_lands_clause_forms(self) -> None:
        for text in ("lands with or after the box", "Land before the other",
                     "it lands before x", "which lands with y"):
            with self.subTest(text=text):
                roadmap = f"## Phase 10: C\n- [ ] {text}\n"
                self.assertEqual(len(check_needs(roadmap, [])), 1)
        for text in ("landslide after", "landed after", "it lands in §10.2"):
            with self.subTest(text=text):
                self.assertEqual(check_needs(f"## Phase 10: C\n- [ ] {text}\n", []), [])
        # Phase 11 boxes, boxes before Phase 0, and prose are not checked.
        roadmap = ("- [ ] lands after nothing\n## Phase 10: C\nprose lands after\n"
                   "## Phase 11: P\n- [ ] lands after x\n")
        self.assertEqual(check_needs(roadmap, []), [])


WAVE_ROADMAP = """## Phase 10: C
- [ ] crit open (F001)
- [x] crit ticked (F001)
- [ ] latent open (F002)
- [ ] root open
- [x] needed once
- [ ] needed twice
- [ ] needed from outside
- [ ] outsider
## Phase 11: P
- [ ] later crit (F001)
"""
WAVE_NEEDS: list[NeedsFile] = [
    (10, [row("crit open", ("needed once",)), row("needed once", ("needed twice",)),
          row("outsider", ("needed from outside",))], ["root open"]),
]


class TestWave(unittest.TestCase):
    def lines(self, roadmap: str, needs: list[NeedsFile] = WAVE_NEEDS) -> list[str]:
        return [f"{b.line} {b.ticked}" for b in wave(1, roadmap, REVIEW, needs)]

    def test_wave_1(self) -> None:
        # An open --closed box, an open root, and an open box two needs away;
        # a box needed only from outside the wave stays out.
        self.assertEqual(self.lines(WAVE_ROADMAP),
                         ["2 False", "3 True", "5 False", "6 True", "7 False"])

    def test_all_ticked(self) -> None:
        roadmap = WAVE_ROADMAP.replace("- [ ] crit open", "- [x] crit open").replace(
            "- [ ] root", "- [x] root").replace("- [ ] needed twice", "- [x] needed twice")
        self.assertTrue(all(x.endswith("True") for x in self.lines(roadmap)))

    def test_other_wave_is_an_error(self) -> None:
        with self.assertRaises(ValueError):
            wave(2, WAVE_ROADMAP, REVIEW, WAVE_NEEDS)


class TestMain(unittest.TestCase):
    def run_main(self, argv: list[str], roadmap: str | None = None) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.ExitStack() as st:
            st.enter_context(contextlib.redirect_stdout(out))
            st.enter_context(contextlib.redirect_stderr(err))
            if roadmap is not None:
                m = st.enter_context(mock.patch.object(check_review_refs, "ROADMAP"))
                m.read_text.return_value = roadmap
                st.enter_context(mock.patch.object(check_review_refs, "load_all_needs",
                                                   return_value=WAVE_NEEDS))
                rv = st.enter_context(mock.patch.object(check_review_refs, "REVIEW"))
                rv.read_text.return_value = REVIEW + "\n#### F003 · x\n\n**Severity:** LOW\n"
            rc = check_review_refs.main(argv)
        return rc, out.getvalue(), err.getvalue()

    def test_real_tree_passes(self) -> None:
        rc, out, err = self.run_main([])
        self.assertEqual((rc, err), (0, ""))
        self.assertIn("check_review_refs: ok", out)

    def test_real_tree_wave_1(self) -> None:
        rc, out, _ = self.run_main(["--print-wave", "1"])
        self.assertEqual(rc, 0)
        prefixes = [" ".join(x[x.index("] ") + 2:].split()[:2]) for x in out.splitlines()]
        self.assertIn("`scripts/check_ticks.py`, which", prefixes)
        self.assertIn("until those", prefixes)

    def test_print_and_fail_wave(self) -> None:
        roadmap = WAVE_ROADMAP + "- [ ] cites F003\n"
        rc, out, err = self.run_main(["--print-wave", "1"], roadmap)
        self.assertEqual((rc, err), (0, ""))
        self.assertEqual(out.splitlines(), [
            "L2 [ ] crit open (F001)", "L3 [x] crit ticked (F001)", "L5 [ ] root open",
            "L6 [x] needed once", "L7 [ ] needed twice",
        ])
        rc, _, err = self.run_main(["--wave", "1"], roadmap)
        self.assertEqual(rc, 1)
        self.assertEqual([x.split(":")[1] for x in err.splitlines()], ["2", "5", "7"])
        done = roadmap.replace("- [ ] crit open", "- [x] crit open").replace(
            "- [ ] root", "- [x] root").replace("- [ ] needed twice", "- [x] needed twice")
        self.assertEqual(self.run_main(["--wave", "1"], done)[0], 0)

    def test_other_wave_is_a_usage_error(self) -> None:
        with self.assertRaises(SystemExit) as cm:
            self.run_main(["--wave", "2"])
        self.assertEqual(cm.exception.code, 2)


def placement_roadmap(*phases: tuple[int, str]) -> str:
    """A roadmap with one `### N.1` section per (phase, body)."""
    out = []
    for n, body in phases:
        out.append(f"## Phase {n}: P{n}\n\n### {n}.1 S\n\n{body}\n")
    return "\n".join(out)


def finding(fid: str, severity: str = "LOW", tag: str | None = None) -> Finding:
    return Finding(fid, severity, tag is not None, tag)


class TestPlacement(unittest.TestCase):
    def errs(self, findings: list[Finding], roadmap: str,
             departures: dict[str, tuple[str, str]] | None = None) -> list[str]:
        with mock.patch.dict(DEPARTURES, departures or {}, clear=True):
            return check_placement({f.fid: f for f in findings}, parse_boxes(roadmap))

    def test_prose_only_fails(self) -> None:
        roadmap = placement_roadmap((10, "prose names F001\n- [ ] a box"))
        self.assertEqual(self.errs([finding("F001")], roadmap),
                         ["F001: cited by no box line, only by prose"])

    def test_box_citation_passes(self) -> None:
        roadmap = placement_roadmap((10, "- [ ] a box (F001)"))
        self.assertEqual(self.errs([finding("F001")], roadmap), [])

    def test_critical_after_phase_10_fails(self) -> None:
        roadmap = placement_roadmap((10, "- [ ] other"), (11, "- [ ] a box (F001)"))
        got = self.errs([finding("F001", "CRITICAL")], roadmap)
        self.assertEqual(len(got), 1)
        self.assertIn("F001 (CRITICAL): no box before", got[0])

    def test_critical_in_phase_10_passes(self) -> None:
        roadmap = placement_roadmap((10, "- [x] a box (F001)"), (11, "- [ ] again F001"))
        self.assertEqual(self.errs([finding("F001", "CRITICAL")], roadmap), [])

    def test_latent_critical_after_phase_10_passes(self) -> None:
        roadmap = placement_roadmap((13, "- [ ] a box (F001)"))
        self.assertEqual(self.errs([finding("F001", "HIGH", "threads")], roadmap), [])

    def test_latent_after_its_phase_fails(self) -> None:
        roadmap = placement_roadmap((13, "- [ ] a box (F001)"))
        got = self.errs([finding("F001", "MEDIUM", "Phase 12 COW")], roadmap)
        self.assertEqual(len(got), 1)
        self.assertIn("F001: LATENT tag names Phase 12", got[0])

    def test_latent_in_or_before_its_phase_passes(self) -> None:
        for n in (11, 12):
            with self.subTest(phase=n):
                roadmap = placement_roadmap((n, "- [ ] a box (F001)"), (13, "- [ ] F001"))
                self.assertEqual(self.errs([finding("F001", "MEDIUM", "Phase 12 COW")],
                                           roadmap), [])

    def test_exit_gate_and_stretch_lines_count(self) -> None:
        roadmap = ("## Phase 12: P\n\n**Exit gate.**\n\n- [ ] gate (F001)\n\n"
                   "### Stretch\n\n- [ ] stretch (F002)\n")
        got = self.errs([finding("F001", tag="Phase 12"), finding("F002", tag="Phase 12")],
                        roadmap)
        self.assertEqual(got, [])

    def test_tag_phase(self) -> None:
        cases: list[tuple[str | None, int | None]] = [
            ("Phase 12.5 unified page cache / production disk mounts", 12),
            ("real hardware or KVM; untrusted users, Phase 18/22", 18),
            ("real hardware, ROADMAP §20.1", 20),
            ("Phase 12.7 swap, Phase 18.2 KASLR, Phase 18.3 FSGSBASE", 12),
            ("Phase 22 release: 22.1 \"no paths\"; §19.6", 19),
            ("Phase 20: Real hardware (ECAM / q35)", 20),
            ("real hardware / KVM", None),
            (None, None),
        ]
        for tag, want in cases:
            with self.subTest(tag=tag):
                self.assertEqual(tag_phase(tag), want)

    def test_nested_parentheses_in_tag(self) -> None:
        review = ("#### F118 · x\n\n**Severity:** LOW · LATENT (Phase 20: Real hardware "
                  "(ECAM / q35)) · **Confidence:** Confirmed\n\n#### F119 · y\n\n"
                  "**Severity:** HIGH · **Confidence:** Likely\n")
        f = parse_review(review)
        self.assertEqual(f["F118"].tag, "Phase 20: Real hardware (ECAM / q35)")
        self.assertTrue(f["F118"].latent)
        self.assertIsNone(f["F119"].tag)

    def test_listed_departure_passes(self) -> None:
        roadmap = placement_roadmap((20, "- [ ] other"), (26, "- [ ] the far box (F001)"))
        deps = {"F001": ("the far box", "why")}
        self.assertEqual(self.errs([finding("F001", tag="Phase 20")], roadmap, deps), [])

    def test_stale_departure_fails(self) -> None:
        roadmap = placement_roadmap((20, "- [ ] the near box (F001)"))
        deps = {"F001": ("the near box", "why")}
        got = self.errs([finding("F001", tag="Phase 20")], roadmap, deps)
        self.assertEqual(got, ["DEPARTURES F001: stale, the finding passes without the entry"])

    def test_departure_box_not_citing_fails(self) -> None:
        roadmap = placement_roadmap((20, "- [ ] a box"), (26, "- [ ] the far box (F001)"))
        deps = {"F001": ("a box", "why")}
        got = self.errs([finding("F001", tag="Phase 20")], roadmap, deps)
        self.assertEqual(len(got), 1)
        self.assertIn("does not cite F001", got[0])

    def test_departure_key_unmatched_or_ambiguous_fails(self) -> None:
        roadmap = placement_roadmap((26, "- [ ] far box one (F001)\n- [ ] far box two"))
        for key, word in (("no such box", "matches no line"), ("far box", "matches 2 lines")):
            with self.subTest(key=key):
                got = self.errs([finding("F001", tag="Phase 20")], roadmap,
                                {"F001": (key, "why")})
                self.assertEqual(len(got), 1)
                self.assertIn(word, got[0])

    def test_own_rule_box_and_funded_line_do_not_cite(self) -> None:
        roadmap = (placement_roadmap((10, "- [ ] `scripts/check_review_refs.py` cites F001"))
                   + "\n# Funded goals\n\n### A goal\n\n- [ ] proposal (F001)\n")
        self.assertEqual(self.errs([finding("F001")], roadmap),
                         ["F001: cited by no box line, only by prose"])

    def test_real_tree_only_f122_departs(self) -> None:
        findings = parse_review(REVIEW_PATH.read_text(encoding="utf-8"))
        boxes = roadmap_boxes()
        self.assertEqual(sorted(DEPARTURES), ["F122"])
        self.assertEqual(check_placement(findings, boxes), [])
        with mock.patch.dict(DEPARTURES, {}, clear=True):
            got = check_placement(findings, boxes)
        self.assertEqual(len(got), 1)
        self.assertTrue(got[0].startswith("F122: LATENT tag names Phase 20"), got)


REVIEWS = """# Design reviews

| Id | Severity | Decision | Sections | PR |
|----|----------|----------|----------|----|
| G001 | Major | one | {sections} | #92 |

## H

| Id | Severity | Decision | PR | Sections |
|----|----------|----------|----|----------|
| H007 | Legal | two | #93 | README |
"""

DOC_FILES = {
    "docs/DESIGN.md": "# vibeOS design\n\n## Contents\n\n# 1. Overview\n\n### 1.3 Module map\n",
    "docs/INVARIANTS.md": "# 2. Invariants\n\n### 2.10 Trust boundaries\n",
    "docs/ROADMAP.md": ("# Roadmap\n\n## How to read this\n\n**Standing gates** apply.\n\n"
                        "## Phase 10: Consolidation\n\n### 10.9 Engineering system\n"),
    "docs/SYSCALL.md": "# Syscall ABI\n\n## 1. Registers\n\n```\n### 1.9 fenced\n```\n",
}


class TestDesignReviews(unittest.TestCase):
    def errs(self, cites: dict[str, str], sections: str = "DESIGN §2.10") -> list[str]:
        return check_design_reviews(cites, REVIEWS.format(sections=sections), DOC_FILES)

    def test_unknown_id_fails(self) -> None:
        got = self.errs({"docs/X.md": "see\ndesign review G002 here"})
        self.assertEqual(got, ["docs/X.md:2: design review G002 has no row in DESIGN_REVIEWS.md"])

    def test_known_id_passes(self) -> None:
        for text in ("design review G001", "[design review H007](reviews/x.md)",
                     "Design\nReview G001", "per the DESIGN  REVIEW H007"):
            with self.subTest(text=text):
                self.assertEqual(self.errs({"AGENTS.md": text}), [])

    def test_no_id_is_ignored(self) -> None:
        self.assertEqual(self.errs({"README.md": "A design review writes each decision. "
                                                 "design review g001, design review G0012"}),
                         [])

    def test_missing_section_fails(self) -> None:
        got = self.errs({}, "DESIGN §2.10, §2.11")
        self.assertEqual(got, ["DESIGN_REVIEWS.md:5: G001: DESIGN §2.11 names no heading"])

    def test_fenced_heading_does_not_resolve(self) -> None:
        got = self.errs({}, "SYSCALL §1.9")
        self.assertEqual(got, ["DESIGN_REVIEWS.md:5: G001: SYSCALL §1.9 names no heading"])

    def test_items_resolve(self) -> None:
        sections = ("DESIGN §1.3, §2.10, §2, Contents, header; ROADMAP §10, §10.9, Phase 10, "
                    "How to read this, standing gates, Engineering system; SYSCALL §1, header; "
                    "DESIGN; VIBEFS header")
        self.assertEqual(self.errs({}, sections), [])

    def test_unknown_items_fail(self) -> None:
        got = self.errs({}, "ROADMAP Phase 11, §11, Nowhere")
        self.assertEqual([e.split(": ", 2)[2] for e in got], [
            "ROADMAP Phase 11 names no heading", "ROADMAP §11 names no heading",
            "ROADMAP Nowhere names no heading",
        ])

    def test_bare_section_is_a_phase_only_in_roadmap(self) -> None:
        got = self.errs({}, "SYSCALL §10")
        self.assertEqual(got, ["DESIGN_REVIEWS.md:5: G001: SYSCALL §10 names no heading"])

    def test_other_groups_are_ignored(self) -> None:
        self.assertEqual(self.errs({}, "AGENTS rule 2; issue plan A3; README; CHANGELOG"), [])

    def test_doc_headings(self) -> None:
        got = doc_headings("DESIGN", DOC_FILES)
        self.assertEqual([(h.file, h.line, h.level, h.number, h.title) for h in got], [
            ("docs/DESIGN.md", 1, 1, None, "vibeOS design"),
            ("docs/DESIGN.md", 3, 2, None, "Contents"),
            ("docs/DESIGN.md", 5, 1, "1", "1. Overview"),
            ("docs/DESIGN.md", 7, 3, "1.3", "1.3 Module map"),
            ("docs/INVARIANTS.md", 1, 1, "2", "2. Invariants"),
            ("docs/INVARIANTS.md", 3, 3, "2.10", "2.10 Trust boundaries"),
        ])

    def test_real_tree_passes(self) -> None:
        cites, reviews, files = design_review_inputs()
        self.assertIn("AGENTS.md", cites)
        self.assertIn("docs/INVARIANTS.md", files)
        self.assertEqual(check_design_reviews(cites, reviews, files), [])


if __name__ == "__main__":
    unittest.main()
