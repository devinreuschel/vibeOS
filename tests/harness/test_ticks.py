"""Host tests for scripts/check_ticks.py (every tick names its proof)."""

from __future__ import annotations

import contextlib
import io
import json
import os
import re
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from scripts import check_ticks, gatelib
from scripts.check_ticks import Gh, History, Report, check
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
- [ ] zeta box: the test fails before the fix lands
- [ ] eta box whose text says its test fails before the fix
"""

FILES = {
    "docs/ROADMAP.md": ROADMAP,
    "Makefile": ("lint:\n\techo lint\n\ntest-kernel: iso\n\techo kernel\n\n"
                 "test-unit:\n\techo unit\n"),
    "src/ktest.rs": ("const TESTS: &[Test] = &[\n    (\"legacy_row\", test_legacy),\n"
                     "    test(\n        \"suite_row\",\n        test_suite,\n    ),\n];\n\n"
                     "fn test_legacy() -> Outcome {\n    Outcome::Ok\n}\n\n"
                     "fn test_suite() -> Outcome {\n    Outcome::Ok\n}\n\n"
                     "fn test_calls() -> Outcome {\n"
                     "    asm!(\"int3\", options(nomem));\n"
                     "    fail(\"handler\", Some(vec));\n    Outcome::Ok\n}\n"),
    "src/boot.rs": ("fn boot() {\n    marker!(\"vibeOS: boot: {} cpus up\", n);\n"
                    "    marker!(marker::READY);\n}\n"),
    "src/marker.rs": "pub const READY: &str = \"vibeOS: ready\";\n",
    "src/proc.rs": ("pub fn reaper() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n"
                    "    fn reaper_for_init_state() {\n        assert!(true);\n    }\n}\n"),
    "user/tests/src/main.rs": "fn user_dup() {\n    dup();\n}\n",
    "crates/core/src/a.rs": ("#[cfg(test)]\nmod tests {\n    #[test]\n    fn host_one() {\n"
                             "        assert!(true);\n    }\n\n    #[test]\n    #[ignore]\n"
                             "    fn host_ignored() {\n    }\n\n    fn not_a_test() {\n    }\n\n"
                             "    /// Checks we ignore stale entries.\n    #[test]\n"
                             "    #[cfg_attr(miri, ignore)]\n    fn stale() {\n"
                             "        assert!(true);\n    }\n\n"
                             "    #[test]\n    #[ignore = \"slow\"]\n"
                             "    fn host_slow() {\n    }\n\n"
                             "    #[test]\n    fn handler() {\n        assert!(true);\n    }\n}\n"),
    "tests/harness/test_x.py": ("class T:\n    def test_py(self) -> None:\n        x = 1\n\n"
                                "    def other(self) -> None:\n        pass\n\n\n"
                                "M = Marker(\"vibeOS: ready\", \"fixture_label\")\n"),
    # The harness records a marker under its label (C-RESULTS), not its text.
    "tests/harness/harness.py": ("MARKERS = [\n    Marker(\"vibeOS: ready\", \"ready_ok\"),\n"
                                 "    Marker(\n        \"vibeOS: boot: \",\n"
                                 "        \"boot_cpus\",\n"
                                 "        and_contains=(\" cpus up\",),\n    ),\n"
                                 "    Marker(\"vibeOS: boot: \", \"boot_other\", "
                                 "and_contains=(\" other\",)),\n"
                                 "    Marker(f\"vibeOS: cpu{i} up\", f\"cpu{i}_up\"),\n]\n"),
    "scripts/check_x.py": ("def main() -> int:\n    return 0\n\n\n"
                           "def wave(\n    a: int,\n    b: int,\n) -> int:\n"
                           "    x = a + b\n    return x\n"),
}


class StubGh(Gh):
    """`gh` with canned runs: workflow file -> runs; run id -> results, jobs."""

    def __init__(self, runs: dict[str, list[dict[str, Any]]] | None = None,
                 results: dict[object, list[dict[str, Any]]] | None = None,
                 jobs: dict[object, list[dict[str, Any]]] | None = None) -> None:
        self._runs = runs or {}
        self._results = results or {}
        self._jobs = jobs or {}

    def runs(self, workflow_file: str) -> list[dict[str, Any]]:
        return self._runs.get(workflow_file, [])

    def jobs(self, run_id: object) -> list[dict[str, Any]]:
        return self._jobs.get(run_id, [])

    def download_results(self, run_id: object, dest: Path) -> list[dict[str, Any]]:
        return self._results.get(run_id, [])


class StubHistory(History):
    def __init__(self, records: list[dict[str, Any]]) -> None:
        self._recs = records

    def records(self) -> list[dict[str, Any]]:
        return self._recs


def results_file(commit: str, *, tier: str = "test-kernel", dirty: bool = False,
                 ktest: list[str] | None = None, failed: list[str] | None = None,
                 marker: list[str] | None = None,
                 retries: list[dict[str, str]] | None = None) -> dict[str, Any]:
    return {"schema": 1, "commit": commit, "dirty": dirty, "arch": "x86_64", "tier": tier,
            "qemu": [], "ktest": {"passed": ktest or [], "skipped": [], "failed": failed or []},
            "utest": {"passed": [], "skipped": [], "failed": []},
            "marker": {"passed": marker or [], "failed": []}, "retries": retries or []}


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

    def run_check(self, results: list[dict[str, Any]] | None = None, *,
                  run_commit: str | None = None, gh: StubGh | None = None,
                  history: list[dict[str, Any]] | None = None) -> Report:
        results_dir = None
        if results is not None:
            tmp = tempfile.TemporaryDirectory()
            self.addCleanup(tmp.cleanup)
            results_dir = Path(tmp.name)
            for i, r in enumerate(results):
                (results_dir / f"r{i}.json").write_text(json.dumps(r))
        return check(self.base, "HEAD", repo=self.repo.path, results_dir=results_dir,
                     run_commit=run_commit, gh=gh or StubGh(),
                     history=StubHistory(history or []))

    def head(self) -> str:
        return self.repo.git("rev-parse", "HEAD")

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
        self.roadmap = self.roadmap.replace("- [ ] zeta box: the test fails before the fix "
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
        # A multi-line signature: the body past `) -> int:` is the def's.
        ("scripts/check_x.py::wave", "scripts/check_x.py", "x = a + b", "x = b + a"),
        ("wave", "scripts/check_x.py", "    return x", "    return x + 0"),
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

    def test_registry_row_is_not_any_call(self) -> None:
        """`asm!("int3", ...)` and `fail("handler", ...)` in src/ktest.rs are
        not registry rows, so `handler` resolves to the host test."""
        tree = check_ticks.Tree("HEAD", self.repo.path)
        self.assertEqual(check_ticks.resolve("int3", tree)[0], [])
        defs = check_ticks.resolve("handler", tree)[0]
        self.assertEqual([(d.kind, d.path) for d in defs], [("host", "crates/core/src/a.rs")])
        self.assertEqual([d.kind for d in check_ticks.resolve("legacy_row", tree)[0]],
                         ["ktest", "ktest"])

    def test_host_test_in_src_resolves(self) -> None:
        """vibeos-core's sources are src/*.rs, so a #[test] there is a host test."""
        tree = check_ticks.Tree("HEAD", self.repo.path)
        defs = check_ticks.resolve("reaper_for_init_state", tree)[0]
        self.assertEqual([(d.kind, d.path) for d in defs], [("host", "src/proc.rs")])
        self.assertEqual(check_ticks.resolve("reaper", tree)[0], [])

    def test_proof_deleted_by_a_later_commit_is_not_found(self) -> None:
        self.commit(f"t\n\nProves: check_x -- {self.PREFIX}", "delta box",
                    self.change("scripts/check_x.py", "return 0", "return 1"))
        self.commit("rm", files={"scripts/check_x.py": None})
        self.assertErrors(self.run_check(), "proof not found")


PREFIX = "delta box names nothing at"


class TestResultsRule(RepoCase):
    def tick_suite(self) -> str:
        self.commit(f"t\n\nProves: suite_row -- {PREFIX}", "delta box",
                    self.change("src/ktest.rs", "        test_suite,", "        test_suite, "))
        return self.head()

    def test_passed_at_the_head(self) -> None:
        head = self.tick_suite()
        self.assertEqual(self.run_check([results_file(head, ktest=["suite_row"])]).errors, [])

    def test_passed_at_the_run_commit(self) -> None:
        self.tick_suite()
        merge = "0123456789abcdef0123456789abcdef01234567"
        r = self.run_check([results_file(merge, ktest=["suite_row"])], run_commit=merge)
        self.assertEqual(r.errors, [])

    def test_failed_other_commit_or_dirty(self) -> None:
        head = self.tick_suite()
        for res in (results_file(head, failed=["suite_row"]),
                    results_file(self.base, ktest=["suite_row"]),
                    results_file(head, ktest=["suite_row"], dirty=True)):
            with self.subTest(res=res):
                self.assertErrors(self.run_check([res]), "ktest 'suite_row' passed in no "
                                                         "results file at the head")

    def test_marker_proof(self) -> None:
        self.commit(f't\n\nProves: "vibeOS: ready" (existing: x) -- {PREFIX}', "delta box")
        head = self.head()
        self.assertEqual(self.run_check([results_file(head, marker=["ready_ok"])]).errors, [])
        # A test file's Marker is not the harness's table.
        for marker in ([], ["fixture_label"], ["boot_cpus"]):
            with self.subTest(marker=marker):
                self.assertErrors(self.run_check([results_file(head, marker=marker)]),
                                  "marker 'vibeOS: ready'")

    def test_marker_proof_with_runtime_parts(self) -> None:
        self.commit(f't\n\nProves: "vibeOS: boot: 4 cpus up" (existing: x) -- {PREFIX}',
                    "delta box")
        head = self.head()
        self.assertEqual(self.run_check([results_file(head, marker=["boot_cpus"])]).errors, [])
        self.assertErrors(self.run_check([results_file(head, marker=["boot_other"])]),
                          "marker 'vibeOS: boot: 4 cpus up'")

    def test_marker_labels(self) -> None:
        tree = check_ticks.Tree("HEAD", self.repo.path)
        got = [p.pattern for p in check_ticks.marker_labels("vibeOS: cpu3 up", tree)]
        self.assertEqual(len(got), 1)
        self.assertTrue(re.fullmatch(got[0], "cpu3_up"))

    def test_no_results_dir_skips_the_rule(self) -> None:
        self.tick_suite()
        self.assertEqual(self.run_check().errors, [])

    def test_ignored_host_test(self) -> None:
        for name in ("host_ignored", "host_slow", "crates/core/src/a.rs::host_ignored"):
            with self.subTest(name=name):
                self.setUp()
                self.commit(f"t\n\nProves: {name} (existing: x) -- {PREFIX}", "delta box")
                self.assertErrors(self.run_check(), f"host test '{name}' is #[ignore]d")

    def test_ignore_in_a_doc_comment_or_cfg_attr_is_not_ignored(self) -> None:
        for name in ("stale", "crates/core/src/a.rs::stale"):
            with self.subTest(name=name):
                self.setUp()
                self.commit(f"t\n\nProves: {name} (existing: x) -- {PREFIX}", "delta box")
                self.assertEqual(self.run_check().errors, [])

    def test_retry_fails_a_pull_request_that_ticks(self) -> None:
        head = self.tick_suite()
        retry = {"label": "user_syscalls", "failure_line": "timeout at user: dup ok"}
        r = self.run_check([results_file(head, ktest=["suite_row"], retries=[retry])])
        self.assertErrors(r, "test-kernel retried user_syscalls: timeout at user: dup ok")
        self.base = head
        self.commit("no tick", files={"src/boot.rs": "fn boot() {}\n"})
        self.assertEqual(self.run_check([results_file(self.head(), retries=[retry])]).errors,
                         [])


CI_YML = """name: ci
on: [push]
jobs:
  check:
    runs-on: x
    steps:
      - run: make lint  # the fast gate
  e2e:
    runs-on: x
    steps:
      - run: make test-kernel
"""
STRESS_YML = """name: smp-stress
on:
  schedule:
    - cron: "0 6 * * 1"
jobs:
  stress:
    name: high-CPU SMP stress
    runs-on: x
    steps:
      - name: stress
        run: make test-kernel

  nightly-canary:
    name: latest nightly canary
    runs-on: x
    continue-on-error: true
    steps:
      - name: iso + host units
        run: make iso && make test-unit
"""


class TestBrackets(RepoCase):
    def tick_bracket(self, proof: str, bracket: str, extra: dict[str, str | None] | None = None,
                     ) -> str:
        self.commit(f"t\n\nProves: {proof} [{bracket}] (existing: x) -- {PREFIX}",
                    "delta box", extra)
        return self.head()

    def nightly(self, sha: str, event: str = "schedule", rid: int = 7,
                passed: bool = True) -> StubGh:
        run = {"id": rid, "head_sha": sha, "event": event, "conclusion": "success",
               "workflow": "nightly.yml"}
        res = [results_file(sha, ktest=["suite_row"] if passed else [])]
        return StubGh({"nightly.yml": [run]}, {rid: res})

    def test_run_at_the_head(self) -> None:
        head = self.tick_bracket("suite_row", "nightly")
        r = self.run_check([], gh=self.nightly(head))
        self.assertEqual(r.errors, [])
        self.assertTrue(any("passed in run 7" in n for n in r.notes), r.notes)
        self.assertErrors(self.run_check([], gh=self.nightly(head, passed=False)),
                          "suite_row [nightly]: no run")

    def test_run_at_a_docs_only_commit(self) -> None:
        first = self.tick_bracket("suite_row", "nightly")
        self.commit("docs", files={"docs/other.md": "x\n", "CHANGELOG.md": "y\n"})
        self.assertEqual(self.run_check([], gh=self.nightly(first)).errors, [])

    def test_run_at_the_merge_base(self) -> None:
        self.tick_bracket("suite_row", "nightly")
        self.assertEqual(self.run_check([], gh=self.nightly(self.base)).errors, [])

    def test_run_not_docs_only(self) -> None:
        first = self.tick_bracket("suite_row", "nightly")
        self.commit("code", files={"src/boot.rs": "fn boot() {}\n"})
        for sha in (first, self.base):
            with self.subTest(sha=sha):
                self.assertErrors(self.run_check([], gh=self.nightly(sha)), "no run")

    def test_pull_request_run_never_counts(self) -> None:
        head = self.tick_bracket("suite_row", "nightly")
        self.assertErrors(self.run_check([], gh=self.nightly(head, "pull_request")), "no run")

    def test_no_run(self) -> None:
        self.tick_bracket("suite_row", "nightly")
        self.assertErrors(self.run_check([]), "no run")

    def test_brackets_need_the_results_mode(self) -> None:
        self.tick_bracket("suite_row", "nightly")
        self.assertEqual(self.run_check().errors, [])

    def test_dev_host_record(self) -> None:
        head = self.tick_bracket("suite_row", "dev-host")
        rec = {"event": "dev-host", "head_sha": head, "workflow": "dev-host",
               "jobs": [{"name": "gate", "results": [results_file(head, ktest=["suite_row"])]}]}
        self.assertEqual(self.run_check([], history=[rec]).errors, [])
        # A dev-host record is read from ci-history only, never through gh.
        self.assertErrors(self.run_check([], gh=self.nightly(head)), "[dev-host]: no run")
        self.assertErrors(self.run_check([], history=[{**rec, "event": "push"}]), "no run")

    def test_marker_on_a_nightly_run(self) -> None:
        head = self.tick_bracket('"vibeOS: ready"', "nightly")
        run = {"id": 5, "head_sha": head, "event": "schedule", "conclusion": "success"}
        gh = StubGh({"nightly.yml": [run]}, {5: [results_file(head, marker=["ready_ok"])]})
        self.assertEqual(self.run_check([], gh=gh).errors, [])

    def test_ci_history_record(self) -> None:
        wf: dict[str, str | None] = {".github/workflows/ci.yml": CI_YML}
        head = self.tick_bracket("make lint", "ci-history", wf)
        rec = {"event": "push", "head_sha": head, "workflow": "ci", "branch": "main",
               "conclusion": "failure", "jobs": [{"name": "check", "conclusion": "success"},
                                                 {"name": "e2e", "conclusion": "failure"}]}
        self.assertEqual(self.run_check([], history=[rec]).errors, [])
        self.assertErrors(self.run_check([], history=[{**rec, "branch": "x"}]), "no run")
        bad = {**rec, "conclusion": "success",
               "jobs": [{"name": "check", "conclusion": "failure"}]}
        self.assertErrors(self.run_check([], history=[bad]), "no run")

    def weekly(self, head: str, conclusion: str,
               jobs: list[dict[str, Any]]) -> StubGh:
        run = {"id": 3, "head_sha": head, "event": "schedule", "conclusion": conclusion}
        return StubGh({"smp-stress.yml": [run]}, jobs={3: jobs})

    def test_make_target_on_a_weekly_run(self) -> None:
        """A bracketed `make` target is judged by the job that runs it, not by
        the run's conclusion."""
        wf: dict[str, str | None] = {".github/workflows/smp-stress.yml": STRESS_YML}
        head = self.tick_bracket("make test-kernel", "weekly", wf)
        stress_ok = {"name": "high-CPU SMP stress", "conclusion": "success"}
        canary_bad = {"name": "latest nightly canary", "conclusion": "failure"}
        self.assertEqual(self.run_check([], gh=self.weekly(head, "failure",
                                                           [stress_ok, canary_bad])).errors, [])
        self.assertEqual(self.run_check([], gh=self.weekly(
            head, "success", [{**stress_ok, "name": "high-CPU SMP stress (4)"}])).errors, [])
        for jobs in ([], [canary_bad], [{**stress_ok, "conclusion": "failure"}]):
            with self.subTest(jobs=jobs):
                self.assertErrors(self.run_check([], gh=self.weekly(head, "success", jobs)),
                                  "make test-kernel [weekly]: no run")

    def test_make_target_in_a_continue_on_error_job(self) -> None:
        wf: dict[str, str | None] = {".github/workflows/smp-stress.yml": STRESS_YML}
        head = self.tick_bracket("make test-unit", "weekly", wf)
        stress_ok = {"name": "high-CPU SMP stress", "conclusion": "success"}
        canary = {"name": "latest nightly canary", "conclusion": "failure"}
        self.assertErrors(self.run_check([], gh=self.weekly(head, "success",
                                                            [stress_ok, canary])),
                          "make test-unit [weekly]: no run")
        canary["conclusion"] = "success"
        self.assertEqual(self.run_check([], gh=self.weekly(head, "success",
                                                           [stress_ok, canary])).errors, [])

    def test_make_target_no_job_runs(self) -> None:
        wf: dict[str, str | None] = {".github/workflows/smp-stress.yml": STRESS_YML}
        head = self.tick_bracket("make lint", "weekly", wf)
        jobs = [{"name": "high-CPU SMP stress", "conclusion": "success"}]
        self.assertErrors(self.run_check([], gh=self.weekly(head, "success", jobs)),
                          "make lint [weekly]: no run")

    def test_release_job(self) -> None:
        body = "name: release\njobs:\n  build:\n    runs-on: x\n  publish:\n    runs-on: y\n"
        wf: dict[str, str | None] = {".github/workflows/release.yml": body}
        head = self.tick_bracket("release.yml:build", "release", wf)
        rec = {"event": "workflow_dispatch", "head_sha": self.base, "commit": head,
               "workflow": "release", "jobs": [{"name": "build", "conclusion": "success"}]}
        self.assertEqual(self.run_check([], history=[rec]).errors, [])
        bad = {**rec, "jobs": [{"name": "build", "conclusion": "failure"}]}
        self.assertErrors(self.run_check([], history=[bad]), "no run")
        self.assertErrors(self.run_check([], history=[{**rec, "commit": self.base}]), "no run")


NEEDS = """[[box]]
key = "beta box: `make lint`"
needs = ["delta box names"]
"""
CLOSES = """[[box]]
key = "beta box: `make lint`"
closes = ["delta box names"]
"""
BETA = "Proves: make lint -- beta box: `make lint` checks the"
DELTA = f"Proves: make lint (existing: x) -- {PREFIX}"


class TestNeedsAndCloses(RepoCase):
    def test_needs_open_at_the_head(self) -> None:
        self.commit("rows", files={"tests/gates/phase-10-needs.toml": NEEDS})
        sha = self.commit(f"t\n\n{BETA}", "beta box")
        self.assertErrors(self.run_check(), f"{sha[:7]} L6: needs L8, open at the head")
        self.commit(f"t2\n\n{DELTA}", "delta box")
        self.assertEqual(self.run_check().errors, [])

    def test_needs_rule_is_not_bare(self) -> None:
        self.commit("rows", files={"tests/gates/phase-10-needs.toml": NEEDS})
        self.commit(f"t\n\n{BETA}", "beta box")
        r = check(self.base, "HEAD", repo=self.repo.path, full=False,
                  history=StubHistory([]))
        self.assertEqual(r.errors, [])

    def test_closes_pair_ticks_together(self) -> None:
        self.commit("rows", files={"tests/gates/phase-10-needs.toml": CLOSES})
        sha = self.commit(f"t\n\n{BETA}", "beta box")
        self.assertErrors(self.run_check(), f"{sha[:7]} L8: a closes pair ticks together")
        self.repo.git("reset", "-q", "--hard", "HEAD~1")
        self.roadmap = tick(self.roadmap, "delta box")
        self.commit(f"t\n\n{BETA}\n{DELTA}", "beta box")
        self.assertEqual(self.run_check().errors, [])
        # Ticked in two commits: the first leaves its partner open.
        self.repo.git("reset", "-q", "--hard", "HEAD~1")
        self.roadmap = ROADMAP
        self.commit(f"t\n\n{DELTA}", "delta box")
        self.commit(f"t\n\n{BETA}", "beta box")
        self.assertErrors(self.run_check(), "L6: a closes pair ticks together")


ZETA = "zeta box: the test fails before the fix"


class TestFailsBefore(RepoCase):
    def setUp(self) -> None:
        super().setUp()
        self.test_sha = self.commit("test alone",
                                    files={"src/boot.rs": "fn boot() {}\nfn t() {}\n"})

    def fix(self, lines: str) -> str:
        return self.commit(f"fix\n\nProves: make lint (existing: x) -- {ZETA}\n{lines}",
                           "zeta box", {"src/boot.rs": "fn boot() { fixed(); }\nfn t() {}\n"})

    def good(self, sha: str | None = None) -> str:
        short = (sha or self.test_sha)[:12]
        return f'Fails-before: test-kernel {short} "vibeOS: ktest: FAIL t" -- {ZETA}'

    def test_valid_line_and_inert_history(self) -> None:
        self.fix(self.good())
        r = self.run_check()
        self.assertEqual(r.errors, [])
        self.assertTrue(any("history clause inert" in n for n in r.notes))

    def test_missing(self) -> None:
        sha = self.fix("")
        self.assertErrors(self.run_check(), f"{sha[:7]} L10: the box's test fails before the "
                                            "fix, and the commit carries no `Fails-before:`")

    def test_bad_sha(self) -> None:
        self.fix(self.good(self.base))
        self.assertErrors(self.run_check(), "is not an earlier commit of the pull request")

    def test_bad_tier_and_empty_line(self) -> None:
        self.fix(self.good().replace("test-kernel", "test-nothing"))
        self.assertErrors(self.run_check(), "no Makefile rule 'test-nothing'")
        self.repo.git("reset", "-q", "--hard", "HEAD~1")
        self.roadmap = ROADMAP
        self.fix(f'Fails-before: test-kernel {self.test_sha[:12]} "" -- {ZETA}')
        self.assertErrors(self.run_check(), "failure line is empty", "carries no")

    def test_history_clause_active(self) -> None:
        self.fix(self.good())
        other = {"event": "pull_request", "head_sha": "f" * 40, "jobs": []}
        self.assertErrors(self.run_check(history=[other]),
                          "ci-history holds no failed test-kernel run")
        failed = {"event": "pull_request", "head_sha": self.test_sha, "jobs": [
            {"name": "tier", "results": [results_file(self.test_sha, failed=["t"])]}]}
        r = self.run_check(history=[other, failed])
        self.assertEqual(r.errors, [])
        self.assertFalse(any("inert" in n for n in r.notes))

    def test_its_test_fails_before_does_not_trigger(self) -> None:
        self.commit("t\n\nProves: make lint (existing: x) -- eta box whose text says its",
                    "eta box")
        self.assertEqual(self.run_check().errors, [])


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


class TestSummary(RepoCase):
    def test_summary_lists_errors_existing_and_notes(self) -> None:
        self.commit(f"t\n\n{DELTA}", "delta box")
        self.commit("t2", "beta box")
        r = self.run_check()
        text = check_ticks.summary(r, self.base, self.head())
        self.assertIn("2 ticked lines; 1 errors.", text)
        self.assertIn("### Errors", text)
        self.assertIn("### Existing proofs", text)
        self.assertIn("make lint (existing: x)", text)
        self.assertIn("### Notes", text)


class TestHistory(unittest.TestCase):
    def test_reads_records_from_origin(self) -> None:
        origin = TempRepo()
        self.addCleanup(origin.cleanup)
        origin.commit("main")
        origin.git("checkout", "-q", "--orphan", "ci-history")
        origin.commit("records", {
            "runs/ci/1.json": json.dumps({"run_id": 1, "event": "push", "head_sha": "a"}),
            "records/2026-01-01-dev-host-a-b.json": json.dumps(
                {"run_id": 2, "event": "dev-host", "runner": {"cpu_model": "Café M4"}}),
            "README.md": "not a record\n",
        })
        origin.git("checkout", "-q", "main")
        repo = TempRepo()
        self.addCleanup(repo.cleanup)
        repo.commit("x")
        self.assertEqual(History(repo.path).records(), [])  # no remote, no records
        repo.git("remote", "add", "origin", str(origin.path))
        got = sorted(History(repo.path).records(), key=lambda r: r["run_id"])
        self.assertEqual([(r["id"], r["event"]) for r in got], [(1, "push"), (2, "dev-host")])
        self.assertEqual(got[1]["runner"]["cpu_model"], "Café M4")


class TestRealTree(unittest.TestCase):
    def test_docstring_lists_every_mode(self) -> None:
        doc = check_ticks.__doc__ or ""
        for needle in ("needs,\n  closes and Fails-before", "--results DIR [--run-commit SHA]",
                       "results, retry and bracket", "--summary FILE"):
            self.assertIn(needle, doc)

    def test_marker_labels_of_the_harness(self) -> None:
        tree = check_ticks.Tree("HEAD", gatelib.ROOT)
        for text, label in (("vibeOS: heap ok", "heap_ok"), ("vibeOS: shell ready", "shell_ready"),
                            ("vibeOS: pmm: 1024 free 4KiB frames", "pmm_free_frames"),
                            ("vibeOS: sched: cpu2 ready", "sched_cpu2")):
            with self.subTest(text=text):
                labels = check_ticks.marker_labels(text, tree)
                self.assertTrue(any(p.fullmatch(label) for p in labels), labels)

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
