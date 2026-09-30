"""Host tests for scripts/check_gate_inputs.py (ROADMAP §10.9, gate inputs)."""

from __future__ import annotations

import contextlib
import io
import unittest
from pathlib import Path
from unittest import mock

from scripts import check_gate_inputs as cgi
from scripts.check_gate_inputs import (
    INPUTS,
    diff_errors,
    floor_at,
    load_inputs,
    parse_trailer,
    static_errors,
)
from tests.harness.gitfixture import TempRepo

INPUTS_TOML = """[coverage]
crate = "vibeos-core"
floor = 87

[[input]]
glob = "scripts/check_*.py"
pair = "tests/harness/test_{stem}.py"
why = "check scripts"

[[input]]
glob = "tests/gates/*.toml"
why = "gate maps"

[[input]]
path = "tests/harness/skips.toml"
why = "skips"
list = {kind = "skip", table = "skip", entry = "name", classes = ["arch", "accel", "smp"]}

[[input]]
glob = "tests/gates/xfail.toml"
why = "expected failures"
list = {kind = "expected-failure", table = "xfail", entry = "name", classes = ["arch", "host"]}

[[input]]
path = "Makefile"
recipe = "check"
why = "make check"

[[input]]
path = "pyproject.toml"
table = "tool.ruff"
why = "ruff"

[[input]]
path = "docs/reviews/KERNEL_REVIEW.md"
review = true
why = "review"

[[input]]
glob = ".github/workflows/*"
why = "workflows"
"""

REVIEW = """# Review

#### F001 · first

**Severity:** CRITICAL · **Confidence:** Confirmed

#### F002 · second

**Severity:** LOW · **Confidence:** Likely

## 8. Guardrails

text
"""

SKIPS = """[[skip]]
name = "a"
reason = "needs 4 cpus"
smp = [1, 2]
"""

XFAIL = """[[xfail]]
name = "old"
reason = "known"
"""

GATE_MAP = """[[line]]
key = "one"
[[line.entry]]
cmd = "make check"
[[line.entry]]
cmd = "python3 scripts/check_x.py"
"""

MAKEFILE = "check:\n\tpython3 a.py\n\ngate:\n\tpython3 scripts/gate.py\n"
PYPROJECT = '[tool.ruff]\nline-length = 100\n\n[tool.mypy]\nstrict = true\n'
CI = """name: ci
jobs:
  check:
    steps:
      - run: echo
"""

BASE: dict[str, str] = {
    INPUTS: INPUTS_TOML,
    "scripts/check_x.py": "print('x')\n",
    "tests/harness/test_x.py": "# test x\n",
    "tests/gates/phase-10.toml": GATE_MAP,
    "tests/harness/skips.toml": SKIPS,
    "tests/gates/xfail.toml": XFAIL,
    "Makefile": MAKEFILE,
    "pyproject.toml": PYPROJECT,
    "docs/reviews/KERNEL_REVIEW.md": REVIEW,
    ".github/workflows/ci.yml": CI,
}


class DictTree:
    def __init__(self, files: dict[str, str]) -> None:
        self.data = {k: v.encode() for k, v in files.items()}

    def read(self, path: str) -> bytes | None:
        return self.data.get(path)

    def files(self) -> list[str]:
        return sorted(self.data)


def changes(base: dict[str, str], head: dict[str, str]) -> list[tuple[str, str]]:
    out = []
    for p in sorted(set(base) | set(head)):
        if p not in head:
            out.append(("D", p))
        elif p not in base:
            out.append(("A", p))
        elif base[p] != head[p]:
            out.append(("M", p))
    return out


def run(edits: dict[str, str | None], trailers: list[str] | None = None,
        base: dict[str, str] | None = None) -> list[str]:
    b = dict(BASE if base is None else base)
    h = dict(b)
    for k, v in edits.items():
        if v is None:
            h.pop(k, None)
        else:
            h[k] = v
    return diff_errors(DictTree(b), DictTree(h), changes(b, h), trailers or [])


def floor(n: int) -> str:
    return INPUTS_TOML.replace("floor = 87", f"floor = {n}")


class TestPlants(unittest.TestCase):
    def test_lowered_floor_fails(self) -> None:
        errs = run({INPUTS: floor(80)})
        self.assertTrue(any("floor lowered from 87 to 80" in e for e in errs), errs)

    def test_lowered_floor_with_trailer_still_fails(self) -> None:
        errs = run({INPUTS: floor(80)}, [f"{INPUTS}: coverage -- slower CI"])
        self.assertTrue(any("floor lowered" in e for e in errs), errs)

    def test_unexplained_expected_failure_entry_fails(self) -> None:
        errs = run({"tests/gates/xfail.toml": XFAIL + '\n[[xfail]]\nname = "new"\nreason = "r"\n'})
        self.assertTrue(any("expected-failure entry 'new'" in e for e in errs), errs)

    def test_expected_failure_entry_with_trailer_passes(self) -> None:
        errs = run({"tests/gates/xfail.toml": XFAIL + '\n[[xfail]]\nname = "new"\nreason = "r"\n'},
                   ["tests/gates/xfail.toml new host: flaky on macOS until §10.2's fix"])
        self.assertEqual(errs, [])

    def test_edited_check_script_fails(self) -> None:
        errs = run({"scripts/check_x.py": "print('y')\n"})
        self.assertEqual(len(errs), 1)
        self.assertIn("scripts/check_x.py: modified", errs[0])

    def test_edited_check_script_with_trailer_passes(self) -> None:
        errs = run({"scripts/check_x.py": "print('y')\n"},
                   ["scripts/check_x.py: box 1234 -- the rule now covers y"])
        self.assertEqual(errs, [])

    def test_removed_gate_map_entry_fails(self) -> None:
        cut = GATE_MAP.replace('[[line.entry]]\ncmd = "python3 scripts/check_x.py"\n', "")
        errs = run({"tests/gates/phase-10.toml": cut})
        self.assertEqual(len(errs), 1)
        self.assertIn("tests/gates/phase-10.toml: modified", errs[0])

    def test_removed_gate_map_entry_with_trailer_passes(self) -> None:
        cut = GATE_MAP.replace('[[line.entry]]\ncmd = "python3 scripts/check_x.py"\n', "")
        errs = run({"tests/gates/phase-10.toml": cut},
                   ["tests/gates/phase-10.toml: exit line one -- make check runs check_x.py"])
        self.assertEqual(errs, [])

    def test_edited_severity_line_fails(self) -> None:
        edited = REVIEW.replace("**Severity:** CRITICAL", "**Severity:** MEDIUM")
        errs = run({"docs/reviews/KERNEL_REVIEW.md": edited})
        self.assertTrue(any("outside `## Errata`" in e for e in errs), errs)

    def test_edited_severity_line_with_trailer_still_fails(self) -> None:
        edited = REVIEW.replace("**Severity:** CRITICAL", "**Severity:** MEDIUM")
        errs = run({"docs/reviews/KERNEL_REVIEW.md": edited},
                   ["docs/reviews/KERNEL_REVIEW.md: F001 -- it is medium"])
        self.assertEqual(len(errs), 1)
        self.assertIn("outside `## Errata`", errs[0])


class TestNoTrailerNeeded(unittest.TestCase):
    def test_added_input_passes(self) -> None:
        more = INPUTS_TOML + '\n[[input]]\npath = "scripts/gate.py"\nwhy = "make gate"\n'
        self.assertEqual(run({INPUTS: more, "scripts/gate.py": "print()\n"}), [])

    def test_new_check_script_passes(self) -> None:
        self.assertEqual(run({"scripts/check_new.py": "print()\n",
                              "tests/harness/test_new.py": "# t\n"}), [])

    def test_raised_floor_passes(self) -> None:
        self.assertEqual(run({INPUTS: floor(90)}), [])

    def test_removed_expected_failure_entry_passes(self) -> None:
        self.assertEqual(run({"tests/gates/xfail.toml": ""}), [])


class TestLists(unittest.TestCase):
    NEW = SKIPS + '\n[[skip]]\nname = "b"\nreason = "no kvm"\naccel = "tcg"\n'

    def test_skip_entry_trailer_names_entry_class_reason(self) -> None:
        path = "tests/harness/skips.toml"
        self.assertTrue(run({path: self.NEW}))
        self.assertTrue(run({path: self.NEW}, [f"{path} b accel:"]))
        self.assertTrue(run({path: self.NEW}, [f"{path} c accel: other entry"]))
        self.assertEqual(run({path: self.NEW}, [f"{path} b accel: needs KVM"]), [])

    def test_skip_entry_wrong_class_fails(self) -> None:
        path = "tests/harness/skips.toml"
        errs = run({path: self.NEW}, [f"{path} b smp: needs KVM"])
        self.assertTrue(any("skip entry 'b'" in e for e in errs), errs)
        errs = run({path: self.NEW}, [f"{path} b mem: needs KVM"])
        self.assertTrue(errs)

    def test_changed_skip_entry_counts_as_added(self) -> None:
        path = "tests/harness/skips.toml"
        changed = SKIPS.replace("needs 4 cpus", "needs 4 CPUs")
        errs = run({path: changed})
        self.assertTrue(any("skip entry 'a'" in e for e in errs), errs)
        self.assertEqual(run({path: changed}, [f"{path} a smp: the reason's new text"]), [])

    def test_new_list_file_rows_need_trailers(self) -> None:
        base = dict(BASE)
        del base["tests/gates/xfail.toml"]
        errs = run({"tests/gates/xfail.toml": XFAIL}, base=base)
        self.assertTrue(any("expected-failure entry 'old'" in e for e in errs), errs)
        ok = run({"tests/gates/xfail.toml": XFAIL}, ["tests/gates/xfail.toml old arch: known"],
                 base=base)
        self.assertEqual(ok, [])


ERRATA = "\n## Errata\n\n- 2026-10-01 · F001 · **Severity:** MEDIUM -- misread the window\n"


class TestReview(unittest.TestCase):
    def test_appended_erratum_with_trailer_passes(self) -> None:
        errs = run({"docs/reviews/KERNEL_REVIEW.md": REVIEW + ERRATA},
                   ["docs/reviews/KERNEL_REVIEW.md: F001's severity -- erratum"])
        self.assertEqual(errs, [])

    def test_appended_erratum_without_trailer_fails(self) -> None:
        errs = run({"docs/reviews/KERNEL_REVIEW.md": REVIEW + ERRATA})
        self.assertEqual(len(errs), 1)
        self.assertIn("changed with no", errs[0])

    def test_edited_heading_fails(self) -> None:
        edited = REVIEW.replace("#### F002 · second", "#### F002 · renamed")
        errs = run({"docs/reviews/KERNEL_REVIEW.md": edited},
                   ["docs/reviews/KERNEL_REVIEW.md: F002 -- title"])
        self.assertTrue(any("outside `## Errata`" in e for e in errs), errs)


class TestInputsToml(unittest.TestCase):
    def test_floor_moves_from_ci_yml(self) -> None:
        base = dict(BASE)
        del base[INPUTS]
        base[".github/workflows/ci.yml"] = CI + "      - run: llvm-cov --fail-under-lines 87\n"
        self.assertEqual(floor_at(DictTree(base)), 87)
        self.assertEqual(run({INPUTS: INPUTS_TOML, ".github/workflows/ci.yml": CI},
                             [".github/workflows/ci.yml: floor -- moves to inputs.toml"],
                             base=base), [])

    def test_floor_moved_lower_fails(self) -> None:
        base = dict(BASE)
        del base[INPUTS]
        base[".github/workflows/ci.yml"] = CI + "      - run: llvm-cov --fail-under-lines 87\n"
        errs = run({INPUTS: floor(86), ".github/workflows/ci.yml": CI},
                   [".github/workflows/ci.yml: floor -- moves"], base=base)
        self.assertTrue(any("floor lowered from 87 to 86" in e for e in errs), errs)

    def test_removed_input_entry_needs_trailer(self) -> None:
        cut = INPUTS_TOML.replace('[[input]]\npath = "pyproject.toml"\ntable = "tool.ruff"\n'
                                  'why = "ruff"\n', "")
        self.assertNotEqual(cut, INPUTS_TOML)
        errs = run({INPUTS: cut})
        self.assertTrue(any("entry 'pyproject.toml' removed" in e for e in errs), errs)
        self.assertEqual(run({INPUTS: cut}, [f"{INPUTS}: gate inputs -- ruff is gone"]), [])

    def test_recipe_edit_needs_trailer(self) -> None:
        edited = MAKEFILE.replace("python3 a.py", "python3 b.py")
        errs = run({"Makefile": edited})
        self.assertEqual(len(errs), 1)
        self.assertIn("`check` recipe: changed", errs[0])
        self.assertEqual(run({"Makefile": edited}, ["Makefile: make check -- runs b"]), [])
        other = MAKEFILE + "\nrun:\n\tqemu\n"
        self.assertEqual(run({"Makefile": other}), [])

    def test_table_edit_needs_trailer(self) -> None:
        edited = PYPROJECT.replace("100", "120")
        errs = run({"pyproject.toml": edited})
        self.assertEqual(len(errs), 1)
        self.assertIn("pyproject.toml [tool.ruff]: changed", errs[0])
        self.assertEqual(run({"pyproject.toml": PYPROJECT.replace("strict = true",
                                                                   "strict = false")}), [])

    def _static(self, files: dict[str, str]) -> list[str]:
        return static_errors(Path("/nonexistent"), DictTree(files))

    def test_stale_path_fails(self) -> None:
        files = dict(BASE)
        del files["pyproject.toml"]
        errs = self._static(files)
        self.assertIn(f"{INPUTS}: path 'pyproject.toml' does not exist", errs)

    def test_required_inputs_listed(self) -> None:
        files = dict(BASE)
        files["scripts/gate.py"] = ""
        files["deny.toml"] = ""
        errs = self._static(files)
        self.assertIn(f"{INPUTS}: scripts/gate.py is a gate input and is not listed", errs)
        self.assertIn(f"{INPUTS}: deny.toml is a gate input and is not listed", errs)
        self.assertIn(f"{INPUTS}: the recipe Makefile 'gate' is not listed", errs)
        self.assertIn(f"{INPUTS}: the table pyproject.toml 'tool.mypy' is not listed", errs)

    def test_literal_floor_in_workflow_fails(self) -> None:
        files = dict(BASE)
        files[".github/workflows/ci.yml"] = CI + "      - run: x --fail-under-lines 87\n"
        errs = self._static(files)
        self.assertTrue(any(e.startswith(".github/workflows/ci.yml:6:") for e in errs), errs)

    def test_schema(self) -> None:
        with self.assertRaises(cgi.InputsError):
            load_inputs(INPUTS_TOML.replace("floor = 87", "floor = 101"))
        with self.assertRaises(cgi.InputsError):
            load_inputs(INPUTS_TOML + '\n[[input]]\npath = "a"\nglob = "b"\nwhy = "x"\n')
        with self.assertRaises(cgi.InputsError):
            load_inputs(INPUTS_TOML + '\n[[input]]\npath = "a"\npair = "t_{stem}"\nwhy = "x"\n')

    def test_real_tree_is_clean(self) -> None:
        self.assertEqual(static_errors(cgi.ROOT), [])


class TestTrailer(unittest.TestCase):
    def test_forms(self) -> None:
        t = parse_trailer("a/b.py: rule -- why")
        self.assertIsNotNone(t)
        assert t is not None
        self.assertEqual((t.path, t.entry), ("a/b.py", None))
        e = parse_trailer("tests/harness/skips.toml foo smp: reason")
        assert e is not None
        self.assertEqual((e.path, e.entry, e.cls), ("tests/harness/skips.toml", "foo", "smp"))
        self.assertIsNone(parse_trailer("a/b.py: no reason given"))
        self.assertIsNone(parse_trailer("a/b.py: -- why"))
        self.assertIsNone(parse_trailer("a b: reason"))
        self.assertIsNone(parse_trailer("no colon"))


class TestGit(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = TempRepo()
        self.addCleanup(self.repo.cleanup)
        self.base = self.repo.commit("base", dict(BASE))

    def diff(self) -> tuple[list[str], list[str]]:
        return cgi.run_diff(self.repo.path, self.base, "HEAD")

    def test_trailer_read_anywhere_in_message(self) -> None:
        self.repo.commit("edit\n\nGate-change: scripts/check_x.py: box -- why\n\nmore text\n",
                         {"scripts/check_x.py": "print('y')\n"})
        self.assertEqual(self.diff()[0], [])

    def test_trailer_in_other_pr_commit_counts(self) -> None:
        self.repo.commit("note\n\nGate-change: scripts/check_x.py: box -- why")
        self.repo.commit("edit", {"scripts/check_x.py": "print('y')\n"})
        self.assertEqual(self.diff()[0], [])
        self.repo.commit("unparsed\n\nGate-change: scripts/check_x.py bad")
        errs, warnings = self.diff()
        self.assertEqual(errs, [])
        self.assertEqual(len(warnings), 1)

    def test_no_origin_main_skips_diff_rules(self) -> None:
        self.repo.commit("edit", {"scripts/check_x.py": "print('y')\n"})
        out = io.StringIO()
        with mock.patch.object(cgi, "ROOT", self.repo.path), \
                mock.patch.object(cgi, "static_errors", return_value=[]), \
                contextlib.redirect_stdout(out):
            rc = cgi.main([])
        self.assertEqual(rc, 0)
        self.assertIn("static rules only: no origin/main", out.getvalue())
        self.repo.git("update-ref", "refs/remotes/origin/main", self.base)
        err = io.StringIO()
        with mock.patch.object(cgi, "ROOT", self.repo.path), \
                mock.patch.object(cgi, "static_errors", return_value=[]), \
                contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            rc = cgi.main([])
        self.assertEqual(rc, 1)
        self.assertIn("scripts/check_x.py: modified", err.getvalue())


class TestSummary(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = TempRepo()
        self.addCleanup(self.repo.cleanup)

    def test_summary_lists_changed_inputs_and_trailers(self) -> None:
        self.repo.commit("base", dict(BASE))
        self.repo.git("tag", "-a", "v0.8.0", "-m", "0.8.0")
        self.repo.commit("edit\n\nGate-change: scripts/check_x.py: box 1 -- y\n"
                         "Gate-change: pyproject.toml: box 2 -- lint",
                         {"scripts/check_x.py": "print('y')\n",
                          "pyproject.toml": PYPROJECT.replace("100", "120")})
        self.repo.commit("floor", {INPUTS: floor(88), "src.rs": "fn main() {}\n"})
        self.repo.git("tag", "-a", "v0.9.0", "-m", "0.9.0")
        out = io.StringIO()
        with mock.patch.object(cgi, "ROOT", self.repo.path), contextlib.redirect_stdout(out):
            rc = cgi.main(["--summary", "--tag", "v0.9.0"])
        self.assertEqual(rc, 0)
        text = out.getvalue()
        self.assertIn("## Gate inputs changed since v0.8.0", text)
        self.assertIn("Coverage floor: 87 at v0.8.0, 88 at", text)
        items = [x for x in text.splitlines() if x.startswith("- ")]
        self.assertEqual(items, ["- pyproject.toml [tool.ruff]", "- scripts/check_x.py",
                                 f"- {INPUTS}"])
        self.assertIn("Gate-change: scripts/check_x.py: box 1 -- y", text)
        self.assertIn("Gate-change: pyproject.toml: box 2 -- lint", text)
        self.assertIn("no `Gate-change:` line", text)
        self.assertNotIn("src.rs", text)

    def test_summary_without_previous_tag(self) -> None:
        self.repo.commit("base\n\nGate-change: scripts/check_x.py: new, box -- first",
                         dict(BASE))
        self.repo.git("tag", "-a", "v0.8.0", "-m", "0.8.0")
        self.assertIsNone(cgi.previous_tag(self.repo.path, "v0.8.0"))
        text = cgi.summary(self.repo.path, None, "v0.8.0")
        self.assertIn("since the root of history", text)
        self.assertIn("Coverage floor: None at the root of history, 87 at", text)
        self.assertIn("- scripts/check_x.py", text)
        self.assertIn("Gate-change: scripts/check_x.py: new, box -- first", text)


CI_PASS_YML = """name: ci
jobs:
  check:
    runs-on: ubuntu-26.04
  build:
    runs-on: ubuntu-26.04
  ticks:
    if: ${{ github.event_name == 'pull_request' && !cancelled() }}
    needs: [build]
  ci-pass:
    if: always()
    needs: [check, build, ticks]
    steps:
      - name: every job succeeded
        env:
          NEEDS: ${{ toJSON(needs) }}
          SKIPPABLE: ticks
        run: python3 scripts/check_gate_inputs.py --ci-pass
"""


class TestCiPass(unittest.TestCase):
    def test_needs_every_job(self) -> None:
        self.assertEqual(cgi.ci_pass_errors(CI_PASS_YML), [])
        real = (cgi.ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertEqual(cgi.ci_pass_errors(real), [])

    def test_missing_job_fails(self) -> None:
        errs = cgi.ci_pass_errors(CI_PASS_YML.replace("[check, build, ticks]", "[check, ticks]"))
        self.assertEqual(len(errs), 1)
        self.assertIn("does not need job `build`", errs[0])
        real = (cgi.ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        cut = real.replace("needs: [check, build, tier, ticks]", "needs: [check, build, ticks]")
        self.assertNotEqual(cut, real)
        self.assertTrue(any("does not need job `tier`" in e for e in cgi.ci_pass_errors(cut)))
        files = dict(BASE)
        files[".github/workflows/ci.yml"] = cut
        errs = static_errors(Path("/nonexistent"), DictTree(files))
        self.assertTrue(any("does not need job `tier`" in e for e in errs), errs)
        errs = cgi.ci_pass_errors(CI_PASS_YML.replace("  ci-pass:", "  other:"))
        self.assertIn("no `ci-pass` job", errs[0])

    def test_requires_always(self) -> None:
        for cond in ("${{ !cancelled() }}", "success()"):
            errs = cgi.ci_pass_errors(CI_PASS_YML.replace("if: always()", f"if: {cond}"))
            self.assertEqual(len(errs), 1, cond)
            self.assertIn("lacks `if: always()`", errs[0])
        self.assertEqual(cgi.ci_pass_errors(
            CI_PASS_YML.replace("if: always()", "if: ${{ always() }}")), [])

    def test_skippable_must_be_event_gated(self) -> None:
        errs = cgi.ci_pass_errors(CI_PASS_YML.replace("SKIPPABLE: ticks", "SKIPPABLE: ticks build"))
        self.assertEqual(len(errs), 1)
        self.assertIn("SKIPPABLE names `build`", errs[0])
        errs = cgi.ci_pass_errors(CI_PASS_YML.replace("SKIPPABLE: ticks", "SKIPPABLE: nope"))
        self.assertIn("which is no job", errs[0])

    def test_verdict_success(self) -> None:
        needs = {"check": {"result": "success", "outputs": {}},
                 "ticks": {"result": "success", "outputs": {}}}
        self.assertEqual(cgi.ci_pass_verdict(needs, {"ticks"}), [])
        self.assertEqual(cgi.ci_pass_verdict(needs, set()), [])

    def test_verdict_failure_fails(self) -> None:
        needs = {"check": {"result": "failure"}, "build": {"result": "success"}}
        self.assertEqual(cgi.ci_pass_verdict(needs, set()),
                         ["ci-pass: job `check` concluded 'failure'"])
        self.assertTrue(cgi.ci_pass_verdict({}, set()))

    def test_verdict_skipped_only_when_skippable(self) -> None:
        needs = {"check": {"result": "success"}, "ticks": {"result": "skipped"}}
        self.assertEqual(cgi.ci_pass_verdict(needs, {"ticks"}), [])
        self.assertTrue(cgi.ci_pass_verdict(needs, set()))
        tier = {"check": {"result": "failure"}, "tier": {"result": "skipped"}}
        self.assertEqual(len(cgi.ci_pass_verdict(tier, {"ticks"})), 2)

    def test_verdict_cancelled_fails(self) -> None:
        needs = {"ticks": {"result": "cancelled"}}
        self.assertEqual(cgi.ci_pass_verdict(needs, {"ticks"}),
                         ["ci-pass: job `ticks` concluded 'cancelled'"])
        env = {"NEEDS": '{"tier": {"result": "cancelled"}}', "SKIPPABLE": "ticks"}
        with mock.patch.dict("os.environ", env), \
                contextlib.redirect_stderr(io.StringIO()) as err:
            self.assertEqual(cgi.main(["--ci-pass"]), 1)
        self.assertIn("`tier` concluded 'cancelled'", err.getvalue())


if __name__ == "__main__":
    unittest.main()
