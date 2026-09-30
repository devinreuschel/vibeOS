"""Host tests for scripts/release_check.py, release.yml's gate (ROADMAP §10.1)."""

from __future__ import annotations

import contextlib
import io
import unittest
from typing import Any

from scripts.release_check import ReleaseError, check, main

REPO = "o/r"
C = "c" * 40
TAG_OBJ = "a" * 40
PHASE_OBJ = "b" * 40
TREE = "e" * 40
R = f"repos/{REPO}"


def good() -> dict[str, Any]:
    """An annotated v0.10.0 and phase-10 on commit C on main, with a green push run."""
    return {
        f"{R}/git/ref/tags/v0.10.0": {
            "ref": "refs/tags/v0.10.0",
            "object": {"type": "tag", "sha": TAG_OBJ},
        },
        f"{R}/git/tags/{TAG_OBJ}": {"object": {"type": "commit", "sha": C}},
        f"{R}/compare/{C}...main": {"status": "ahead"},
        f"{R}/actions/workflows/ci.yml/runs?head_sha={C}&per_page=100": {
            "workflow_runs": [
                {"event": "pull_request", "head_sha": C, "conclusion": "success"},
                {"event": "push", "head_sha": C, "conclusion": "success"},
            ]
        },
        f"{R}/git/matching-refs/tags/phase-": [
            {"ref": "refs/tags/phase-10", "object": {"type": "tag", "sha": PHASE_OBJ}},
        ],
        f"{R}/git/ref/tags/phase-10": {
            "ref": "refs/tags/phase-10",
            "object": {"type": "tag", "sha": PHASE_OBJ},
        },
        f"{R}/git/tags/{PHASE_OBJ}": {"object": {"type": "commit", "sha": C}},
    }


def fake(table: dict[str, Any]) -> Any:
    def api(path: str) -> Any:
        if path not in table:
            raise ReleaseError(f"gh api {path}: HTTP 404")
        return table[path]

    return api


def runs(*items: dict[str, Any]) -> dict[str, Any]:
    return {"workflow_runs": list(items)}


RUNS = f"{R}/actions/workflows/ci.yml/runs?head_sha={C}&per_page=100"


class CheckTest(unittest.TestCase):
    def fails(self, table: dict[str, Any], tag: str, needle: str) -> None:
        with self.assertRaises(ReleaseError) as cm:
            check(fake(table), REPO, tag)
        self.assertIn(needle, str(cm.exception))

    def test_passes_annotated_tag_on_main_with_proving_run(self) -> None:
        self.assertEqual(check(fake(good()), REPO, "v0.10.0"), C)
        t = good()
        t[f"{R}/compare/{C}...main"] = {"status": "identical"}
        self.assertEqual(check(fake(t), REPO, "v0.10.0"), C)

    def test_bad_tag_form(self) -> None:
        for tag in ("0.10.0", "v0.10", "v0.10.0-rc1", "phase-10", "v0.10.0\n"):
            self.fails(good(), tag, "is not v<major>.<minor>.<patch>")

    def test_missing_tag(self) -> None:
        self.fails(good(), "v0.11.0", "404")

    def test_lightweight_tag(self) -> None:
        t = good()
        t[f"{R}/git/ref/tags/v0.10.0"]["object"] = {"type": "commit", "sha": C}
        self.fails(t, "v0.10.0", "is not annotated")

    def test_tag_on_a_tree(self) -> None:
        t = good()
        t[f"{R}/git/tags/{TAG_OBJ}"] = {"object": {"type": "tree", "sha": TREE}}
        self.fails(t, "v0.10.0", "points at a tree")

    def test_commit_not_on_main(self) -> None:
        for status in ("behind", "diverged"):
            t = good()
            t[f"{R}/compare/{C}...main"] = {"status": status}
            self.fails(t, "v0.10.0", f"is not on main (compare: {status})")

    def test_no_ci_run(self) -> None:
        t = good()
        t[RUNS] = runs()
        self.fails(t, "v0.10.0", "no successful ci run proves")

    def test_failed_run(self) -> None:
        t = good()
        t[RUNS] = runs({"event": "push", "head_sha": C, "conclusion": "failure"})
        self.fails(t, "v0.10.0", "docs/RELEASING.md")

    def test_success_run_that_does_not_prove_the_commit(self) -> None:
        for run in (
            {"event": "pull_request", "head_sha": C, "conclusion": "success"},
            {"event": "push", "head_sha": "d" * 40, "conclusion": "success"},
        ):
            t = good()
            t[RUNS] = runs(run)
            self.fails(t, "v0.10.0", "no successful ci run proves")


class MainTest(unittest.TestCase):
    def run_main(self, table: dict[str, Any]) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = main(["--repo", REPO, "--tag", "v0.10.0"], api=fake(table))
        return rc, out.getvalue(), err.getvalue()

    def test_prints_commit_and_phase(self) -> None:
        self.assertEqual(self.run_main(good()), (0, f"commit={C}\nphase=10\n", ""))

    def test_failure_prints_reason_and_exits_1(self) -> None:
        t = good()
        t[f"{R}/compare/{C}...main"] = {"status": "diverged"}
        rc, out, err = self.run_main(t)
        self.assertEqual((rc, out), (1, ""))
        self.assertTrue(err.startswith("release: commit "), err)

    def test_phase_tag_required_once(self) -> None:
        t = good()
        t[f"{R}/git/matching-refs/tags/phase-"] = []
        rc, out, err = self.run_main(t)
        self.assertEqual((rc, out), (1, ""))
        self.assertIn("no annotated phase-<N> tags", err)
        t = good()
        other = "f" * 40
        t[f"{R}/git/matching-refs/tags/phase-"].append(
            {"ref": "refs/tags/phase-9", "object": {"type": "tag", "sha": other}}
        )
        t[f"{R}/git/ref/tags/phase-9"] = {
            "ref": "refs/tags/phase-9",
            "object": {"type": "tag", "sha": other},
        }
        t[f"{R}/git/tags/{other}"] = {"object": {"type": "commit", "sha": C}}
        rc, _, err = self.run_main(t)
        self.assertEqual(rc, 1)
        self.assertIn("2 annotated phase-<N> tags", err)

    def test_phase_tag_on_another_commit_is_ignored(self) -> None:
        t = good()
        other = "f" * 40
        t[f"{R}/git/matching-refs/tags/phase-"].append(
            {"ref": "refs/tags/phase-9", "object": {"type": "tag", "sha": other}}
        )
        t[f"{R}/git/ref/tags/phase-9"] = {
            "ref": "refs/tags/phase-9",
            "object": {"type": "tag", "sha": other},
        }
        t[f"{R}/git/tags/{other}"] = {"object": {"type": "commit", "sha": "9" * 40}}
        self.assertEqual(self.run_main(t)[0], 0)


if __name__ == "__main__":
    unittest.main()
