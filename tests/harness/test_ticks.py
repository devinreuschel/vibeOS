"""Host tests for scripts/check_ticks.py (every tick names its proof)."""

from __future__ import annotations

import contextlib
import io
import os
import unittest
from unittest import mock

from scripts import check_ticks, gatelib
from scripts.check_ticks import Report, check
from tests.harness.gitfixture import TempRepo

ROADMAP = """# Roadmap
## Phase 10: Consolidation
### 10.1 Boxes
- [ ] alpha box one two three four five
- [ ] alpha box one two three four six
- [ ] beta box: `make lint` checks the tree
- [ ] gamma box runs `cargo clippy --bin vibeos -- -D warnings` everywhere
- [ ] delta box names nothing at all today
- [x] epsilon box was ticked before the pull request
- [ ] zeta box whose test fails before the fix lands
"""

FILES = {
    "docs/ROADMAP.md": ROADMAP,
    "Makefile": "lint:\n\techo lint\n\ntest-kernel: iso\n\techo kernel\n",
    "src/ktest.rs": ("const TESTS: &[Test] = &[\n    (\"legacy_row\", test_legacy),\n"
                     "    test(\n        \"suite_row\",\n        test_suite,\n    ),\n];\n\n"
                     "fn test_legacy() -> Outcome {\n    Outcome::Ok\n}\n\n"
                     "fn test_suite() -> Outcome {\n    Outcome::Ok\n}\n"),
    "src/boot.rs": ("fn boot() {\n    marker!(\"vibeOS: boot: {} cpus up\", n);\n"
                    "    marker!(marker::READY);\n}\n"),
    "src/marker.rs": "pub const READY: &str = \"vibeOS: ready\";\n",
    "user/tests/src/main.rs": "fn user_dup() {\n    dup();\n}\n",
    "crates/core/src/a.rs": ("#[cfg(test)]\nmod tests {\n    #[test]\n    fn host_one() {\n"
                             "        assert!(true);\n    }\n\n    #[test]\n    #[ignore]\n"
                             "    fn host_ignored() {\n    }\n\n    fn not_a_test() {\n    }\n}\n"),
    "tests/harness/test_x.py": ("class T:\n    def test_py(self) -> None:\n        x = 1\n\n"
                                "    def other(self) -> None:\n        pass\n"),
    "scripts/check_x.py": "def main() -> int:\n    return 0\n",
}


def tick(text: str, needle: str, rest: str = "") -> str:
    """`text` with the box line that starts with `needle` ticked."""
    out = []
    for line in text.splitlines(keepends=True):
        if line.startswith(f"- [ ] {needle}"):
            line = line.replace("- [ ] ", "- [x] ", 1)
            if rest:
                line = line.rstrip("\n") + rest + "\n"
        out.append(line)
    return "".join(out)


class RepoCase(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = TempRepo()
        self.addCleanup(self.repo.cleanup)
        self.base = self.repo.commit("base", dict(FILES))
        self.roadmap = ROADMAP

    def commit(self, message: str, needle: str | None = None,
               files: dict[str, str | None] | None = None) -> str:
        files = dict(files or {})
        if needle is not None:
            self.roadmap = tick(self.roadmap, needle)
        files.setdefault("docs/ROADMAP.md", self.roadmap)
        return self.repo.commit(message, files)

    def run_check(self) -> Report:
        return check(self.base, "HEAD", repo=self.repo.path)

    def assertErrors(self, r: Report, *needles: str) -> None:  # noqa: N802
        self.assertEqual(len(r.errors), len(needles), r.errors)
        for e, n in zip(r.errors, needles, strict=True):
            self.assertIn(n, e)

    def change(self, path: str, old: str, new: str) -> dict[str, str | None]:
        text = (self.repo.path / path).read_text()
        self.assertIn(old, text)
        return {path: text.replace(old, new)}


class TestPairing(RepoCase):
    def test_paired_tick_passes(self) -> None:
        self.commit("t\n\nProves: make lint -- beta box: `make lint` checks the", "beta box")
        r = self.run_check()
        self.assertEqual((r.errors, r.ticks), ([], 1))

    def test_no_pair(self) -> None:
        sha = self.commit("t", "beta box")
        self.assertErrors(self.run_check(), f"{sha[:7]} L6: ticked with no `Proves:` line")

    def test_four_word_prefix(self) -> None:
        self.commit("t\n\nProves: make lint -- beta box: `make lint`", "beta box")
        self.assertErrors(self.run_check(), "fewer than 5 words", "ticked with no")

    def test_prefix_pairs_two_lines(self) -> None:
        self.roadmap = tick(self.roadmap, "alpha box one two three four five")
        self.commit("t\n\nProves: make lint -- alpha box one two three four", "alpha box")
        self.assertErrors(self.run_check(), "pairs with 2 ticked lines (L4, L5)",
                          "ticked with no", "ticked with no")

    def test_proves_with_no_tick(self) -> None:
        self.commit("t\n\nProves: make lint -- delta box names nothing at", "beta box")
        self.assertErrors(self.run_check(), "pairs with no line this commit ticks",
                          "ticked with no")

    def test_proves_in_another_commit_does_not_pair(self) -> None:
        self.commit("t", "beta box")
        self.commit("u\n\nProves: make lint -- beta box: `make lint` checks the")
        self.assertErrors(self.run_check(), "ticked with no", "pairs with no line")

    def test_prefix_containing_the_separator(self) -> None:
        self.commit("t\n\nProves: make lint -- gamma box runs `cargo clippy --bin vibeos -- -D",
                    "gamma box", self.change("Makefile", "echo lint", "echo lint2"))
        self.assertEqual(self.run_check().errors, [])

    def test_two_proves_for_one_box(self) -> None:
        self.commit("t\n\nProves: make lint -- beta box: `make lint` checks the\n"
                    "Proves: legacy_row -- beta box: `make lint` checks the tree", "beta box",
                    self.change("src/ktest.rs", "Outcome::Ok\n}\n\nfn test_suite",
                                "Outcome::Ok // x\n}\n\nfn test_suite"))
        self.assertEqual(self.run_check().errors, [])

    def test_malformed_lines(self) -> None:
        self.commit("t\n\nProves: make lint\nProves:  -- beta box: `make lint` checks the\n"
                    "Proves: make lint [hourly] -- beta box: `make lint` checks the", "beta box")
        self.assertErrors(self.run_check(), "no ` -- <prefix>`", "names no proof",
                          "unknown bracket [hourly]", "ticked with no")

    def test_squash_merge_message(self) -> None:
        self.roadmap = tick(self.roadmap, "delta box")
        msg = ("Phase 10 (#194)\n\n* P10-S03: one\n\nProves: make lint -- beta box: `make "
               "lint` checks the\n\n* P10-S03: two\n\n    Proves: make lint (existing: x) -- "
               "delta box names nothing at\n\nCo-Authored-By: x <x@example.invalid>\n")
        self.commit(msg, "beta box")
        r = self.run_check()
        self.assertEqual((r.errors, r.ticks), ([], 2))


class TestTextChanges(RepoCase):
    def setUp(self) -> None:
        super().setUp()
        self.commit("t\n\nProves: make lint -- beta box: `make lint` checks the", "beta box")

    def test_changed_ticked_text_needs_a_new_proof(self) -> None:
        self.roadmap = self.roadmap.replace("- [x] epsilon box was", "- [x] epsilon box is")
        sha = self.commit("reword")
        self.assertErrors(self.run_check(), f"{sha[:7]} L9: ticked with no")
        self.base = self.repo.git("rev-parse", "HEAD")
        self.roadmap = self.roadmap.replace("- [x] epsilon box is", "- [x] epsilon box now")
        self.commit("reword again\n\nProves: make lint (existing: same) -- epsilon box now "
                    "ticked before the pull")
        self.assertEqual(self.run_check().errors, [])

    def test_moved_line_is_not_a_tick(self) -> None:
        lines = self.roadmap.splitlines(keepends=True)
        eps = [x for x in lines if "epsilon" in x]
        rest = [x for x in lines if "epsilon" not in x]
        self.roadmap = "".join(rest[:3] + eps + rest[3:])
        self.commit("move")
        self.assertEqual(self.run_check().errors, [])

    def test_removed_and_re_added_later_is_not_a_tick(self) -> None:
        eps = next(x for x in self.roadmap.splitlines(keepends=True) if "epsilon" in x)
        self.roadmap = self.roadmap.replace(eps, "")
        self.commit("drop")
        self.roadmap += eps
        self.commit("restore")
        self.assertEqual(self.run_check().errors, [])

    def test_reopen_and_delete_need_no_pair(self) -> None:
        self.roadmap = self.roadmap.replace("- [x] epsilon", "- [ ] epsilon")
        self.roadmap = self.roadmap.replace("- [x] beta box", "- [ ] beta box")
        self.commit("reopen")
        self.roadmap = self.roadmap.replace("- [ ] zeta box whose test fails before the fix "
                                            "lands\n", "")
        self.commit("delete")
        self.assertEqual(self.run_check().errors, [])

    def test_tick_then_text_change_needs_two_proves(self) -> None:
        self.roadmap = tick(self.roadmap, "delta box")
        self.commit("t2\n\nProves: make lint (existing: x) -- delta box names nothing at all")
        self.roadmap = self.roadmap.replace("delta box names nothing at all today",
                                            "delta box names nothing at all tomorrow")
        sha = self.commit("reword")
        self.assertErrors(self.run_check(), f"{sha[:7]} L8: ticked with no")


class TestDiffRule(RepoCase):
    """Each proof kind: changed passes, unchanged fails, `(existing:)` is
    reported, and a name in the box's text passes."""

    KINDS: list[tuple[str, str, str, str]] = [
        # proof, path, old, new
        ("make test-kernel", "Makefile", "echo kernel", "echo kernel2"),
        ("tests/harness/test_x.py", "tests/harness/test_x.py", "x = 1", "x = 2"),
        ("tests/harness/test_x.py::test_py", "tests/harness/test_x.py", "x = 1", "x = 2"),
        ('"vibeOS: boot: 4 cpus up"', "src/boot.rs", ", n);", ", m);"),
        ('"vibeOS: ready"', "src/marker.rs", "ready", "ready"),
        ("legacy_row", "src/ktest.rs", "Outcome::Ok\n}\n\nfn test_suite",
         "Outcome::Ok // x\n}\n\nfn test_suite"),
        ("suite_row", "src/ktest.rs", "        test_suite,", "        test_suite,  "),
        ("suite_row", "src/ktest.rs", "fn test_suite() -> Outcome {\n    Outcome::Ok",
         "fn test_suite() -> Outcome {\n    Outcome::Ok // y"),
        ("user_dup", "user/tests/src/main.rs", "dup();", "dup(); // z"),
        ("host_one", "crates/core/src/a.rs", "assert!(true);", "assert!(!false);"),
        ("test_py", "tests/harness/test_x.py", "x = 1", "x = 2"),
        ("check_x", "scripts/check_x.py", "return 0", "return 0  # z"),
    ]
    PREFIX = "delta box names nothing at"

    def test_changed_passes(self) -> None:
        for proof, path, old, new in self.KINDS:
            if old == new:
                continue
            with self.subTest(proof=proof, old=old):
                self.setUp()
                self.commit(f"t\n\nProves: {proof} -- {self.PREFIX}", "delta box",
                            self.change(path, old, new))
                self.assertEqual(self.run_check().errors, [])

    def test_unchanged_fails(self) -> None:
        for proof, _, _, _ in self.KINDS:
            with self.subTest(proof=proof):
                self.setUp()
                self.commit(f"t\n\nProves: {proof} -- {self.PREFIX}", "delta box",
                            self.change("Makefile", "echo lint", "echo lint3"))
                self.assertErrors(self.run_check(), "is not changed by the pull request")

    def test_a_change_outside_the_definition_fails(self) -> None:
        self.commit(f"t\n\nProves: test_py -- {self.PREFIX}", "delta box",
                    self.change("tests/harness/test_x.py", "pass", "return"))
        self.assertErrors(self.run_check(), "is not changed")

    def test_existing_is_reported(self) -> None:
        self.commit(f"t\n\nProves: host_one (existing: tested since Phase 3) -- {self.PREFIX}",
                    "delta box")
        r = self.run_check()
        self.assertEqual(r.errors, [])
        self.assertEqual(len(r.existing), 1)
        self.assertIn("host_one (existing: tested since Phase 3)", r.existing[0])

    def test_name_in_the_box_text_passes(self) -> None:
        self.commit("t\n\nProves: make lint -- beta box: `make lint` checks the", "beta box")
        self.assertEqual(self.run_check().errors, [])

    def test_proof_not_found(self) -> None:
        for proof in ("make nothing", "tests/harness/none.py", "tests/harness/test_x.py::nope",
                      '"vibeOS: never"', "not_a_def", "not_a_test", "two words"):
            with self.subTest(proof=proof):
                self.setUp()
                self.commit(f"t\n\nProves: {proof} (existing: x) -- {self.PREFIX}", "delta box")
                self.assertErrors(self.run_check(), "proof not found at the head")

    def test_proof_deleted_by_a_later_commit_is_not_found(self) -> None:
        self.commit(f"t\n\nProves: check_x -- {self.PREFIX}", "delta box",
                    self.change("scripts/check_x.py", "return 0", "return 1"))
        self.commit("rm", files={"scripts/check_x.py": None})
        self.assertErrors(self.run_check(), "proof not found")


class TestBareMode(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = TempRepo()
        self.addCleanup(self.repo.cleanup)
        self.repo.commit("base", dict(FILES))

    def run_main(self, argv: list[str]) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err), \
                mock.patch.object(check_ticks, "ROOT", self.repo.path), \
                mock.patch.object(gatelib, "ROOT", self.repo.path), \
                mock.patch.dict(os.environ, {}):
            cwd = os.getcwd()
            os.chdir(self.repo.path)
            try:
                rc = check_ticks.main(argv)
            finally:
                os.chdir(cwd)
        return rc, out.getvalue(), err.getvalue()

    def test_no_origin_main_skips(self) -> None:
        self.assertEqual(self.run_main([]), (0, "check_ticks: skipped (no origin/main)\n", ""))

    def test_origin_main_is_the_base(self) -> None:
        self.repo.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.repo.commit("t", {"docs/ROADMAP.md": tick(ROADMAP, "beta box")})
        rc, _, err = self.run_main([])
        self.assertEqual(rc, 1)
        self.assertIn("ticked with no `Proves:` line", err)
        self.repo.git("reset", "-q", "--hard", "HEAD~1")
        self.repo.commit("t\n\nProves: make lint -- beta box: `make lint` checks the",
                         {"docs/ROADMAP.md": tick(ROADMAP, "beta box")})
        self.assertEqual(self.run_main([])[0], 0)


class TestRealTree(unittest.TestCase):
    def test_parse_proves_splits_at_the_first_separator(self) -> None:
        p = check_ticks.parse_proves("make check -- CI runs `cargo clippy --bin vibeos -- -D")
        assert isinstance(p, check_ticks.ProvesLine)
        self.assertEqual((p.proof, p.prefix), ("make check",
                                               "CI runs `cargo clippy --bin vibeos -- -D"))
        p = check_ticks.parse_proves("lifetime_x [nightly] (existing: y (z)) -- a b c d e")
        assert isinstance(p, check_ticks.ProvesLine)
        self.assertEqual((p.proof, p.bracket, p.existing), ("lifetime_x", "nightly", "y (z)"))


if __name__ == "__main__":
    unittest.main()
