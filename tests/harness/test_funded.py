"""Host tests for scripts/check_funded.py (the Funded goals section, ROADMAP §10.9)."""

from __future__ import annotations

import contextlib
import io
import unittest

from scripts import check_funded
from scripts.check_funded import check_funded as check

HEAD = """# Roadmap

## How to read this

The free resources: beyond their own agent tokens, nothing. A gate needs no machine.

## Phase 20: Hardware

- [ ] the box line holds a boxed phrase here

# Funded goals

vibeOS has no budget. The first goal's edit: {preamble}

**Names.** In the lines below, *the x86_64 test PC* and *the
rig host* are machines.

## Bare metal

"""

GOAL = """### {heading}

**{kind}.** A thing. **Cost.** About $1.

**Unlocks.** More.

**Lines.** Elsewhere in this file:
{lines}

"""

PREAMBLE = '"beyond their own agent tokens" gains "and the goals met".'


def goal(heading: str, lines: str = "- nothing", kind: str = "Buy") -> str:
    return GOAL.format(heading=heading, kind=kind, lines=lines)


def roadmap(*goals: str, preamble: str = PREAMBLE) -> str:
    return HEAD.format(preamble=preamble) + "".join(goals)


class TestParts(unittest.TestCase):
    def test_complete_goal_passes(self) -> None:
        for kind in ("Buy", "Rent", "Open"):
            with self.subTest(kind=kind):
                self.assertEqual(check(roadmap(goal("Used x86_64 test PC", kind=kind))), [])

    def test_missing_buy_rent_open_fails(self) -> None:
        got = check(roadmap(goal("A goal", kind="Price")))
        self.assertEqual(got, ["ROADMAP.md:20: A goal: no **Buy.**, **Rent.**, or **Open.** part"])

    def test_missing_cost_unlocks_lines_fails(self) -> None:
        for part in ("Cost", "Unlocks", "Lines"):
            with self.subTest(part=part):
                text = roadmap(goal("A goal")).replace(f"**{part}.**", "")
                self.assertEqual(check(text), [f"ROADMAP.md:20: A goal: no **{part}.** part"])

    def test_part_mid_paragraph_counts(self) -> None:
        text = roadmap(goal("A goal")).replace("**Cost.** About $1.", "")
        text = text.replace("More.", "More. **Cost.** About $1.")
        self.assertEqual(check(text), [])


class TestPhrases(unittest.TestCase):
    def test_once_passes(self) -> None:
        lines = '- How to read this: "A gate needs no machine" becomes "A gate needs one"'
        self.assertEqual(check(roadmap(goal("G", lines))), [])

    def test_wrapped_phrase_once_passes(self) -> None:
        lines = ('- How to read this: "The free resources: beyond\n'
                 '  their own" becomes "the resources"')
        self.assertEqual(check(roadmap(goal("G", lines))), [])

    def test_zero_or_twice_fails(self) -> None:
        for phrase, n in (("no such words", 0), ("machine", 2)):
            with self.subTest(phrase=phrase):
                got = check(roadmap(goal("G", f'- somewhere: "{phrase}" gains "x"')))
                self.assertEqual(len(got), 1)
                self.assertIn(f'"{phrase}" occurs {n} times', got[0])

    def test_other_lines_and_preamble_quotes_are_ignored(self) -> None:
        lines = '- "A gate needs no machine" becomes "A gate needs one"'
        text = roadmap(goal("G", lines), goal("H", lines),
                       preamble=PREAMBLE + ' Also "A gate needs no machine" there.')
        self.assertEqual(check(text), [])

    def test_checkbox_line_fails(self) -> None:
        got = check(roadmap(goal("G", '- Phase 20: "a boxed phrase" becomes "x"')))
        self.assertEqual(len(got), 1)
        self.assertIn("occurs on a checkbox line, ROADMAP.md:9", got[0])

    def test_box_lines_in_lines_are_not_bullets(self) -> None:
        lines = '- [ ] a new box whose text says "no such words" becomes "x"'
        self.assertEqual(check(roadmap(goal("G", lines))), [])

    def test_preamble_phrase_is_checked(self) -> None:
        got = check(roadmap(preamble='"not in the file" gains "x".'))
        self.assertEqual(len(got), 1)
        self.assertIn('"not in the file" occurs 0 times', got[0])
        self.assertEqual(check(roadmap(preamble='"vibeOS has no budget" becomes "x".')), [])

    def test_earlier_goal_exception(self) -> None:
        first = goal("Used x86_64 test PC",
                     '- How to read this: "A gate needs no machine" gains "and on a PC"')
        second = goal("Aarch64 server",
                      '- How to read this: the test PC goal\'s "and on a PC" becomes "and more"')
        self.assertEqual(check(roadmap(first, second)), [])

    def test_bold_goal_name_exception(self) -> None:
        first = goal("GitHub Pro", '- How to read this: "A gate needs no machine" becomes '
                                   '"A gate needs GitHub Pro"')
        second = goal("GitHub GPU runner", '- the **GitHub Pro** goal is deleted; in prose, '
                                           '"GitHub Pro" becomes "GitHub Team"')
        self.assertEqual(check(roadmap(first, second)), [])

    def test_exception_needs_an_earlier_goal_holding_the_phrase(self) -> None:
        holder = goal("Used x86_64 test PC",
                      '- How to read this: "A gate needs no machine" gains "and on a PC"')
        naming = goal("Aarch64 server",
                      '- How to read this: the test PC goal\'s "and on a PC" becomes "x"')
        lacking = goal("Used x86_64 test PC", "- nothing quoted")
        for text in (roadmap(naming, holder), roadmap(lacking, naming)):
            with self.subTest(text=text[-80:]):
                got = check(text)
                self.assertEqual(len(got), 1)
                self.assertIn('"and on a PC" occurs 0 times', got[0])


class TestNeeds(unittest.TestCase):
    def test_goal_or_names_machine_passes(self) -> None:
        lines = ("- nothing\n\n- [ ] a line (needs: the x86_64 test PC and the rig host)\n"
                 "- [ ] another (needs: Used x86_64 test PC, the Rig host)")
        self.assertEqual(check(roadmap(goal("Used x86_64 test PC", lines))), [])

    def test_unknown_need_fails(self) -> None:
        got = check(roadmap(goal("G", "- nothing\n\n- [ ] a line (needs: a second laptop)")))
        self.assertEqual(len(got), 1)
        self.assertIn("(needs: a second laptop) names neither", got[0])

    def test_need_in_code_span_is_ignored(self) -> None:
        text = roadmap(preamble=PREAMBLE + " A line ends `(needs: <machine>)`.")
        self.assertEqual(check(text), [])


class TestTree(unittest.TestCase):
    def test_real_tree_passes(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(check_funded.main([]), 0)
        self.assertEqual(out.getvalue(), "check_funded: ok\n")


if __name__ == "__main__":
    unittest.main()
