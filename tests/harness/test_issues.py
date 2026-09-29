"""Host tests for scripts/check_issues.py, the review-issue index against the
roadmap (ROADMAP Phase 10 exit gate)."""

from __future__ import annotations

import tempfile
import unittest
from collections.abc import Callable
from pathlib import Path

from scripts import check_issues
from scripts.check_issues import check, citations, cited_codes, parse_index

CODES = {"A2", "P1", "Q1", "R1", "D2", "O1"}

INDEX = """# Review issues

| ID | Title | Status |
|---|---|---|
| [A2](A2-x.md) | Portable | `in progress (#88)` |
| [Q1](Q1-x.md) | Gates | `implemented (#80, #194)` |
| [D2](D2-x.md) | Drivers | `proposed` |
| [O1](O1-x.md) | Note | `superseded: Phase 9 did not pause, so there is no resume` |
"""

ROADMAP = """# Roadmap
## Phase 10: Consolidation
**Exit gate**
- [ ] a check fails as in `(Q1, F147)`, which is an example
- [x] a ticked box (Q1, F147)

### 10.2 Build
- [ ] a portable half (DESIGN §11.1) (A2 landed the crate; `cfg(target_arch)` stays)
- [ ] driver instances (D2)
## Phase 11: Portability
- [ ] a later box (O1)
"""

PLAN = ("# Plan\n\n**ROADMAP:** the §10.2 box that cites it. Superseded here: none.\n\n"
        "| a | b |\n")


def plans(missing: tuple[str, ...] = (), text: str = PLAN) -> Callable[[str], str | None]:
    return lambda name: None if name.split("-")[0] in missing else text


def problems(index: str = INDEX, roadmap: str = ROADMAP, closed: bool = False,
             missing: tuple[str, ...] = (), plan: str = PLAN) -> list[str]:
    rows = parse_index(index)
    return check(rows, citations(roadmap, {r.code for r in rows}), plans(missing, plan), closed)


class Citations(unittest.TestCase):
    def test_code_span_example_does_not_cite(self) -> None:
        self.assertEqual(cited_codes("fails as in `(Q1, F147)`, an example", CODES), set())
        self.assertEqual(cited_codes("fails (Q1, F147)", CODES), {"Q1"})

    def test_list_cites_every_item(self) -> None:
        self.assertEqual(cited_codes("no build product (P1, R1)", CODES), {"P1", "R1"})

    def test_nested_parentheses(self) -> None:
        line = ("then fails on `asm!` or `cfg(target_arch` anywhere (DESIGN §11.1) "
                "(A2 landed the crate and host tests; `thread.rs` still carries "
                "`cfg(target_arch)` and `global_asm!`)")
        self.assertEqual(cited_codes(line, CODES), {"A2"})
        self.assertEqual(cited_codes("(see (A2, D2) there)", CODES), {"A2", "D2"})

    def test_item_must_start_with_the_code(self) -> None:
        self.assertEqual(cited_codes("(the A2 crate, and P1x)", CODES), set())

    def test_open_boxes_before_phase_eleven(self) -> None:
        cites = citations(ROADMAP, CODES)
        self.assertEqual([b.line for b in cites["A2"]], [8])
        self.assertEqual(cites["Q1"], [])  # the ticked box and the code-span example
        self.assertEqual(cites["O1"], [])  # after `## Phase 11:`


class Rules(unittest.TestCase):
    def test_good(self) -> None:
        self.assertEqual(problems(), [])

    def test_bad_cell(self) -> None:
        for cell in ("done", "in progress", "implemented (#)", "in progress (88)",
                     "declined:", "implemented (#1,#2)"):
            got = problems(INDEX.replace("`implemented (#80, #194)`", f"`{cell}`"))
            self.assertTrue(any("is not proposed" in p for p in got), cell)

    def test_implemented_with_open_citing_box(self) -> None:
        got = problems(INDEX.replace("`in progress (#88)`", "`implemented (#88)`"))
        self.assertIn("README.md:5: A2: implemented while open boxes cite it (L8)", got)
        got = problems(INDEX.replace("`in progress (#88)`", "`declined: not needed`"))
        self.assertTrue(any("declined while open boxes cite it" in p for p in got))

    def test_proposed_with_no_citing_box(self) -> None:
        got = problems(roadmap=ROADMAP.replace("- [ ] driver instances (D2)",
                                               "- [x] driver instances (D2)"))
        self.assertIn("README.md:7: D2: proposed but no open box in Phases 0 to 10 cites it",
                      got)

    def test_roadmap_line(self) -> None:
        got = problems(plan="# Plan\n\n| a |\n\n**ROADMAP:** §10.2. Superseded here: none.\n")
        self.assertIn("README.md:5: A2: plan A2-x.md: no **ROADMAP:** line above its first "
                      "table", got)
        for plan in ("**ROADMAP:** the box. Superseded here: none.\n",
                     "**ROADMAP:** the §10.2 box.\n"):
            got = problems(plan=plan)
            self.assertTrue(any("names no § section" in p for p in got), plan)

    def test_missing_plan(self) -> None:
        got = problems(missing=("Q1",))
        self.assertIn("README.md:6: Q1: plan Q1-x.md is missing", got)

    def test_citing_box_deferred_past_ten(self) -> None:
        got = problems(roadmap=ROADMAP.replace("driver instances (D2)",
                                               "driver instances (D2). lands in §11.4."))
        self.assertIn("README.md:7: D2: citing box L9 is deferred past Phase 10 "
                      "(lands in §11.x)", got)

    def test_closed(self) -> None:
        got = problems(closed=True)
        self.assertIn("README.md:5: A2: still in progress (#88)", got)
        self.assertIn("README.md:7: D2: still proposed", got)
        self.assertEqual(len(got), 2)


class Main(unittest.TestCase):
    def test_main_on_a_tree(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            issues = root / "docs" / "reviews" / "issues"
            issues.mkdir(parents=True)
            (issues / "README.md").write_text(INDEX, encoding="utf-8")
            for code in ("A2", "Q1", "D2", "O1"):
                (issues / f"{code}-x.md").write_text(PLAN, encoding="utf-8")
            (root / "docs" / "ROADMAP.md").write_text(ROADMAP, encoding="utf-8")
            self.assertEqual(check_issues.main([], root), 0)
            self.assertEqual(check_issues.main(["--closed"], root), 1)

    def test_repo_index(self) -> None:
        self.assertEqual(check_issues.main([]), 0)


if __name__ == "__main__":
    unittest.main()
