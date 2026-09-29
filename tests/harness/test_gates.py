"""Host tests for scripts/check_gates.py, the gate-map rules (ROADMAP §10.9)."""

from __future__ import annotations

import tempfile
import textwrap
import unittest
from collections.abc import Callable
from pathlib import Path

from scripts import check_gates
from scripts.check_gates import (
    MapError,
    gate_lines,
    lands_in_sections,
    load_map,
    norm,
    strip_code_spans,
    validate,
    workflow_self_hosted,
)

ROADMAP = """# Roadmap
## Phase 9: Users
**Exit gate**
- [x] a nine line
- [ ] tag `phase-9` and release `v0.9.0`

### 9.1 Loader
- [ ] a box, not a gate line
## Phase 10: Consolidation
Prose.

**Exit gate**
- [x] `make check` gates CI
- [ ] `scripts/check_x.py` in `make check` keeps it true, over a long line that
- [ ] the third line
- [ ] tag `phase-10` and release `v0.10.0`

### 10.1 Gates
- [ ] a section box
## Phase 11: Portability
"""

GOOD_MAP = '''
[[line]]
key = """`make check` gates
CI"""
[[line.entry]]
cmd = "make check"

[[line]]
key = "`scripts/check_x.py` in `make check` keeps it true, over a long line that"
[[line.entry]]
cmd = "python3 scripts/check_x.py"
[[line.entry]]
job = { workflow = "ci.yml", job = "check" }

[[line]]
key = "the third line"
[[line.entry]]
record = { cmd = "make models" }
[[line.entry]]
cmd = "false"
expect = "fail"
'''

HOSTED = "jobs:\n  check:\n    runs-on: ubuntu-26.04  # not self-hosted\n"


def workflows(texts: dict[str, str]) -> Callable[[str], str | None]:
    return lambda name: texts.get(name)


def problems(map_text: str, phase: int = 10, wf: dict[str, str] | None = None) -> list[str]:
    return validate(phase, load_map(map_text), gate_lines(ROADMAP, phase),
                    workflows(wf if wf is not None else {"ci.yml": HOSTED}))


class Helpers(unittest.TestCase):
    def test_norm(self) -> None:
        self.assertEqual(norm("  a\n  b\tc  "), "a b c")

    def test_strip_code_spans(self) -> None:
        self.assertEqual(strip_code_spans("x `(Q1, F147)` y"), "x   y")
        self.assertEqual(strip_code_spans("a `b` c `d"), "a   c `d")

    def test_lands_in_sections(self) -> None:
        self.assertEqual(lands_in_sections("foo. lands in §10.2 and §10.7."),
                         [(10, 2), (10, 7)])
        self.assertEqual(lands_in_sections("x lands in §11.2, §12.1, and §13.4"),
                         [(11, 2), (12, 1), (13, 4)])
        self.assertEqual(lands_in_sections("a `lands in §10.x` example"), [])
        self.assertEqual(lands_in_sections("a `lands in §M.x` note"), [])
        self.assertEqual(lands_in_sections("see §10.2; it lands in two slices"), [])

    def test_gate_lines(self) -> None:
        got = gate_lines(ROADMAP, 10)
        self.assertEqual([g.line for g in got], [13, 14, 15, 16])
        self.assertEqual([g.tag for g in got], [False, False, False, True])
        self.assertTrue(got[0].ticked)
        self.assertEqual(got[2].text, "the third line")
        self.assertEqual([g.line for g in gate_lines(ROADMAP, 9)], [4, 5])

    def test_self_hosted(self) -> None:
        self.assertTrue(workflow_self_hosted("jobs:\n  a:\n    runs-on: [self-hosted, linux]\n"))
        self.assertTrue(workflow_self_hosted(
            "jobs:\n  a:\n    runs-on:\n      group: g\n      labels:\n        - self-hosted\n"))
        self.assertTrue(workflow_self_hosted(
            "jobs:\n  a:\n    strategy:\n      matrix:\n        os: [ubuntu-26.04, self-hosted]\n"
            "    runs-on: ${{ matrix.os }}\n"))
        self.assertFalse(workflow_self_hosted(HOSTED))
        self.assertFalse(workflow_self_hosted("# self-hosted runners are not used\njobs: {}\n"))


class Rules(unittest.TestCase):
    def test_good_map(self) -> None:
        self.assertEqual(problems(GOOD_MAP), [])

    def test_multiline_key_matches(self) -> None:
        lines = load_map(GOOD_MAP)
        self.assertEqual(lines[0].key, "`make check` gates CI")

    def test_key_matches_no_line(self) -> None:
        text = GOOD_MAP + '\n[[line]]\nkey = "no such line"\n[[line.entry]]\ncmd = "true"\n'
        self.assertTrue(any("matches no exit-gate line" in p for p in problems(text)))

    def test_key_matches_tag(self) -> None:
        text = GOOD_MAP + ('\n[[line]]\nkey = "tag `phase-10` and release `v0.10.0`"\n'
                           '[[line.entry]]\ncmd = "true"\n')
        self.assertTrue(any("tag line" in p for p in problems(text)))

    def test_duplicate_key(self) -> None:
        text = GOOD_MAP + '\n[[line]]\nkey = "the  third\\nline"\n[[line.entry]]\ncmd = "true"\n'
        with self.assertRaisesRegex(MapError, "duplicate key"):
            load_map(text)

    def test_line_without_entry(self) -> None:
        head = GOOD_MAP.split('[[line]]\nkey = "the third line"')[0]
        self.assertIn("L15 has no entry: the third line", problems(head))
        empty = head + '[[line]]\nkey = "the third line"\n'
        self.assertIn("L15 has no entry: the third line", problems(empty))

    def test_check_script_needs_cmd_or_record(self) -> None:
        jobs_only = GOOD_MAP.replace('cmd = "python3 scripts/check_x.py"',
                                     'job = { workflow = "ci.yml", job = "x" }')
        self.assertIn("L14 names scripts/check_x.py and no cmd or record entry runs it",
                      problems(jobs_only))
        as_record = GOOD_MAP.replace('cmd = "python3 scripts/check_x.py"',
                                     'record = { cmd = "python3 scripts/check_x.py" }')
        self.assertEqual(problems(as_record), [])

    def test_entry_runs_make_gate(self) -> None:
        for cmd in ("make gate PHASE=10", "make -C . gate", "python3 scripts/gate.py --phase 10"):
            text = GOOD_MAP.replace('cmd = "make check"', f'cmd = "{cmd}"')
            self.assertTrue(any("runs make gate" in p for p in problems(text)), cmd)
        self.assertEqual(problems(GOOD_MAP.replace('cmd = "make check"',
                                                   'cmd = "make gatekeeper"')), [])

    def test_entry_kinds(self) -> None:
        none = GOOD_MAP.replace('cmd = "make check"', 'expect = "fail"')
        with self.assertRaisesRegex(MapError, "holds 0 of"):
            load_map(none)
        two = GOOD_MAP.replace('cmd = "make check"',
                               'cmd = "make check"\nrecord = { cmd = "make check" }')
        with self.assertRaisesRegex(MapError, "holds 2 of"):
            load_map(two)
        with self.assertRaisesRegex(MapError, "unknown field"):
            load_map(GOOD_MAP.replace('cmd = "make check"', 'cmd = "make check"\nwhy = "x"'))
        with self.assertRaisesRegex(MapError, "not a non-empty string"):
            load_map(GOOD_MAP.replace('cmd = "make check"', "cmd = 3"))

    def test_expect_pass_refused(self) -> None:
        with self.assertRaisesRegex(MapError, "expect is 'pass'"):
            load_map(GOOD_MAP.replace('expect = "fail"', 'expect = "pass"'))

    def test_self_hosted_job(self) -> None:
        for text in (
            "jobs:\n  check:\n    runs-on: [self-hosted, linux]\n",
            "jobs:\n  check:\n    runs-on:\n      labels: [self-hosted]\n",
            "jobs:\n  check:\n    strategy:\n      matrix:\n        r: [self-hosted]\n"
            "    runs-on: ${{ matrix.r }}\n",
        ):
            got = problems(GOOD_MAP, wf={"ci.yml": text})
            self.assertTrue(any("self-hosted label" in p for p in got), text)
        self.assertEqual(problems(GOOD_MAP, wf={}), [])  # a missing workflow is not this rule's

    def test_phase_below_ten(self) -> None:
        self.assertEqual(problems(GOOD_MAP, phase=9),
                         ["phase 9: Phases 0 to 9 get no gate map"])


class Main(unittest.TestCase):
    def tree(self, files: dict[str, str]) -> Path:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        for rel, text in files.items():
            p = root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text, encoding="utf-8")
        return root

    def test_needs_file_is_not_a_map(self) -> None:
        root = self.tree({
            "docs/ROADMAP.md": ROADMAP,
            "tests/gates/phase-10-needs.toml": '[[box]]\nkey = "x"\n',
            "tests/gates/phase-10.toml": GOOD_MAP,
            ".github/workflows/ci.yml": HOSTED,
        })
        self.assertEqual([p.name for _, p in check_gates.map_files(root / "tests/gates")],
                         ["phase-10.toml"])
        self.assertEqual(check_gates.main([], root), 0)

    def test_phase_nine_map_fails(self) -> None:
        root = self.tree({
            "docs/ROADMAP.md": ROADMAP,
            "tests/gates/phase-9.toml": textwrap.dedent("""
                [[line]]
                key = "a nine line"
                [[line.entry]]
                cmd = "true"
            """),
        })
        self.assertEqual(check_gates.main([], root), 1)

    def test_repo_map_valid(self) -> None:
        self.assertEqual(check_gates.main([]), 0)


if __name__ == "__main__":
    unittest.main()
