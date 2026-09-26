"""Host tests for scripts/gatelib.py, the gate scripts' shared readers."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from scripts import gatelib
from scripts.gatelib import GateError, NeedRow
from tests.harness.gitfixture import TempRepo

ROADMAP = """# Roadmap
- [ ] before any phase
## Phase 9: Users
### 9.1 Loader
- [x] loads an ELF
## Phase 10: Consolidation
- [ ] no section yet
### 10.9 Engineering system
- [ ] a gate box (F001)
  - [x] an indented box
prose that mentions a gate box
### Stretch: extras
- [ ] a stretch box
## Phase 11: Portability
### 11.1 Ports
- [ ] a later box (F001)
# Beyond
- [ ] a proposed line
"""

REVIEW = """#### F001 · first

**Severity:** CRITICAL · **Confidence:** Confirmed

#### F002 · second

**Severity:** HIGH · LATENT (COW) · **Confidence:** Likely
"""


class TestBoxes(unittest.TestCase):
    def test_parse_boxes(self) -> None:
        got = [(b.line, b.ticked, b.text, b.section, b.phase)
               for b in gatelib.parse_boxes(ROADMAP)]
        self.assertEqual(got, [
            (2, False, "before any phase", None, None),
            (5, True, "loads an ELF", "9.1", 9),
            (7, False, "no section yet", None, 10),
            (9, False, "a gate box (F001)", "10.9", 10),
            (10, True, "an indented box", "10.9", 10),
            (13, False, "a stretch box", None, 10),
            (16, False, "a later box (F001)", "11.1", 11),
            (18, False, "a proposed line", None, None),
        ])

    def test_roadmap_boxes_reads_the_tree(self) -> None:
        boxes = gatelib.roadmap_boxes()
        self.assertTrue(any(b.phase == 10 and b.section == "10.9" for b in boxes))

    def test_match_key(self) -> None:
        boxes = gatelib.parse_boxes(ROADMAP)
        lines = ROADMAP.splitlines()
        self.assertEqual(gatelib.match_key("loads an", boxes, lines).line, 5)
        self.assertEqual(gatelib.match_key("stretch", boxes).line, 13)
        with self.assertRaisesRegex(GateError, "matches no line"):
            gatelib.match_key("absent", boxes, lines)
        with self.assertRaisesRegex(GateError, "matches 2 lines"):
            gatelib.match_key("box", boxes, lines[:10])
        with self.assertRaisesRegex(GateError, "not a box"):
            gatelib.match_key("mentions a gate", boxes, lines)
        # Without `lines`, prose is not seen, so a box and a prose line pass.
        self.assertEqual(gatelib.match_key("a gate box", boxes).line, 9)
        with self.assertRaisesRegex(GateError, "matches 2 lines"):
            gatelib.match_key("a gate box", boxes, lines)


class TestNeeds(unittest.TestCase):
    def test_rows_roots_and_optional_lists(self) -> None:
        rows, roots = gatelib.parse_needs(
            '[wave1]\nroots = ["a"]\n'
            '[[box]]\nkey = "k"\ncloses = ["c"]\nwhy = "w"\n'
            '[[box]]\nkey = "m"\nneeds = ["n"]\n'
        )
        self.assertEqual(roots, ["a"])
        self.assertEqual(rows, [NeedRow("k", (), ("c",), "w"), NeedRow("m", ("n",), (), "")])

    def test_unknown_field_fails(self) -> None:
        with self.assertRaisesRegex(GateError, "unknown field need"):
            gatelib.parse_needs('[[box]]\nkey = "k"\nneed = ["n"]\n')
        with self.assertRaisesRegex(GateError, "unknown table"):
            gatelib.parse_needs('[wave2]\nroots = []\n')
        with self.assertRaisesRegex(GateError, "no `key`"):
            gatelib.parse_needs('[[box]]\nneeds = ["n"]\n')
        with self.assertRaisesRegex(GateError, "not a list"):
            gatelib.parse_needs('[[box]]\nkey = "k"\nneeds = "n"\n')

    def test_duplicate_rows_merge(self) -> None:
        rows, _ = gatelib.parse_needs(
            '[[box]]\nkey = "k"\nneeds = ["a", "b"]\nwhy = "one"\n'
            '[[box]]\nkey = "k"\nneeds = ["b", "c"]\ncloses = ["d"]\nwhy = "two"\n'
        )
        self.assertEqual(rows, [NeedRow("k", ("a", "b", "c"), ("d",), "one; two")])

    def test_load_all_needs(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            Path(d, "phase-11-needs.toml").write_text('[[box]]\nkey = "x"\n')
            Path(d, "phase-10-needs.toml").write_text('[wave1]\nroots = ["r"]\n')
            Path(d, "phase-10.toml").write_text("not a needs file")
            got = gatelib.load_all_needs(Path(d))
        self.assertEqual(got, [(10, [], ["r"]), (11, [NeedRow("x", (), (), "")], [])])

    def test_real_needs_files_load(self) -> None:
        self.assertEqual([p for p, _, _ in gatelib.load_all_needs()][:2], [10, 11])

    def test_wave1_lines(self) -> None:
        text = ("## Phase 10: C\n"
                "- [ ] crit (F001)\n"      # 2: --closed seed
                "- [x] done crit (F001)\n"  # 3: ticked seeds count too
                "- [ ] latent (F002)\n"     # 4: LATENT: not a seed
                "- [ ] root box\n"          # 5: a [wave1] root
                "- [ ] needed by crit\n"    # 6
                "- [ ] needed twice away\n"  # 7
                "- [ ] needs crit\n"        # 8: needs a wave box, not needed by one
                "## Phase 11: P\n"
                "- [ ] later crit (F001)\n")  # 10: out of scope
        rows10 = [NeedRow("[ ] crit (F001)", ("needed by crit",), (), ""),
                  NeedRow("needs crit", ("done crit",), (), "")]
        rows11 = [NeedRow("needed by crit", ("needed twice",), (), "")]
        got = gatelib.wave1_lines(roadmap_text=text, review_text=REVIEW,
                                  needs=[(10, rows10, ["root box"]), (11, rows11, [])])
        self.assertEqual(got, {2, 3, 5, 6, 7})


class TestReview(unittest.TestCase):
    def test_parse_review(self) -> None:
        f = gatelib.parse_review(REVIEW)
        self.assertEqual((f["F001"].severity, f["F001"].latent), ("CRITICAL", False))
        self.assertTrue(f["F002"].latent)


class TestGit(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = TempRepo()
        self.addCleanup(self.repo.cleanup)

    def test_message_lines(self) -> None:
        sha = self.repo.commit("subj\n\nProves: a -- b\n  Proves: indented -- x\n"
                               "Not Proves: c\n\nTrailer: y\nProves: last -- z")
        self.assertEqual(gatelib.message_lines(sha, "Proves", self.repo.path),
                         ["a -- b", "indented -- x", "last -- z"])
        self.assertEqual(gatelib.message_lines(sha, "Gone", self.repo.path), [])

    def test_pr_commits_and_docs_only(self) -> None:
        r = self.repo
        base = r.commit("base", {"src/a.rs": "a\n", "docs/d.md": "d\n"})
        c1 = r.commit("c1", {"src/a.rs": "b\n"})
        r.git("checkout", "-q", "-b", "side", base)
        s1 = r.commit("s1", {"docs/d.md": "e\n", "CHANGELOG.md": "x\n"})
        r.git("checkout", "-q", "main")
        r.git("merge", "-q", "--no-ff", "-m", "merge", "side")
        c2 = r.commit("c2", {"docs/d.md": "f\n"})
        got = gatelib.pr_commits(base, "HEAD", r.path)
        self.assertEqual(got[-1], c2)
        self.assertEqual(sorted(got[:2]), sorted([c1, s1]))
        self.assertEqual(len(got), 3)
        self.assertTrue(gatelib.docs_only(c1, c2, r.path))
        self.assertTrue(gatelib.docs_only(c2, c2, r.path))
        self.assertFalse(gatelib.docs_only(base, c2, r.path))

    def test_runs(self) -> None:
        r = self.repo
        base = r.commit("base", {"src/a.rs": "a\n"})
        code = r.commit("code", {"src/a.rs": "b\n"})
        docs = r.commit("docs", {"docs/x.md": "x\n"})
        head = r.commit("head", {"CHANGELOG.md": "c\n"})
        pr = [code, docs, head]

        def run(sha: str, event: str = "schedule", **kw: str) -> dict[str, str]:
            return {"event": event, "head_sha": sha, **kw}

        self.assertTrue(gatelib.run_proves_commit(run(head), head))
        self.assertTrue(gatelib.run_proves_commit(run(head, "push"), head))
        self.assertFalse(gatelib.run_proves_commit(run(head, "pull_request"), head))
        self.assertFalse(gatelib.run_proves_commit(run(code), head))
        # A workflow that takes a commit as input names it in its record.
        dispatched = run(code, "workflow_dispatch", commit=head)
        self.assertTrue(gatelib.run_proves_commit(dispatched, head))
        self.assertFalse(gatelib.run_proves_commit(dispatched, code))

        def counts(x: dict[str, str], mb: str | None = None) -> bool:
            return gatelib.run_counts_for_pr(x, pr, head, merge_base=mb, repo=r.path)

        self.assertTrue(counts(run(head)))
        self.assertTrue(counts(run(docs)))  # differs from head only in CHANGELOG.md
        self.assertTrue(counts(run(code)))  # docs/ and CHANGELOG.md only
        self.assertFalse(counts(run(head, "pull_request")))
        self.assertFalse(counts(run(base)))
        self.assertFalse(counts(run(base), mb=base))  # src/ differs
        mb = code
        self.assertTrue(gatelib.run_counts_for_pr(run(mb), [docs, head], head,
                                                  merge_base=mb, repo=r.path))
        self.assertFalse(gatelib.run_counts_for_pr(run(mb), [docs, head], head,
                                                   repo=r.path))
        # C-GATELIB's keyword names.
        self.assertTrue(gatelib.run_counts_for_pr(run(docs), pr_commits=pr, head=head,
                                                  merge_base=None, repo=r.path))
        self.assertTrue(gatelib.commit_counts_for_pr(docs, pr_commits=pr, head=head,
                                                     repo=r.path))


class TestResultsAndBrackets(unittest.TestCase):
    def test_parse_bracket(self) -> None:
        for b in ("nightly", "weekly", "macos", "release", "dev-host", "ci-history"):
            self.assertEqual(gatelib.parse_bracket(f"proof_name [{b}]"), b)
        self.assertEqual(gatelib.parse_bracket("release.yml:build [release]"), "release")
        self.assertIsNone(gatelib.parse_bracket("proof_name"))
        self.assertIsNone(gatelib.parse_bracket('"vibeOS: a [x] b"'))
        with self.assertRaisesRegex(GateError, "unknown bracket"):
            gatelib.parse_bracket("proof_name [hourly]")

    def test_load_results(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            nested = Path(d, "results-x86_64-phase0")
            nested.mkdir()
            (nested / "x86_64-test-kernel.json").write_text(
                json.dumps({"schema": 1, "tier": "test-kernel"}))
            Path(d, "x86_64-test-e2e.json").write_text(json.dumps({"schema": 1, "tier": "e"}))
            got = gatelib.load_results(Path(d))
            self.assertEqual(sorted(x["tier"] for x in got), ["e", "test-kernel"])
            Path(d, "bad.json").write_text("{")
            with self.assertRaisesRegex(GateError, "not a results file"):
                gatelib.load_results(Path(d))
            Path(d, "bad.json").write_text("[]")
            with self.assertRaisesRegex(GateError, "schema-1"):
                gatelib.load_results(Path(d))
        self.assertEqual(gatelib.load_results(Path(d)), [])  # gone: no files


if __name__ == "__main__":
    unittest.main()
