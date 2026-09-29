#!/usr/bin/env python3
"""CI history (ROADMAP §10.9, DESIGN §8.6 CI history).

One JSON record per completed run of an allowlisted workflow, on the orphan
`ci-history` branch, which outlives GitHub's 90-day limit on Actions logs and
artifacts. Records live at `runs/<workflow>/<run_id>.json`, one file per run
id; a later attempt of the run replaces its file.

Modes (one `if` branch each in `main`):

- default: the completeness check. It fails, naming each, on any `ci` run on
  `main` since the history landed that has no record, and on a tombstoned run
  a gate entry needs as proof of the gated commit.
- `--event PATH`: the `workflow_run` writer. It reads only the run id from the
  event file, checks the workflow's name, path and repository against
  `WORKFLOWS`, and builds the record from the Actions API.
- `--backfill [--limit N]`: records, or tombstones, for listed `ci` runs on
  `main` that neither the branch nor an archive holds, newest first.
- `--series WORKFLOW [--job NAME] [--step NAME]`: one line per record (finished
  time, head SHA, run id, seconds), then the median. With no `--job`, the
  run's push-to-green time (§10.1): its latest job `completed` minus its
  earliest job `created`.

Trust rules: a fork's pull request sets a run's branch, title and artifacts, so
no run field reaches a shell line (subprocesses take argv lists), commit
messages and paths are built from the allowlisted workflow key and the integer
run id, and artifacts are untrusted bytes: capped, read in memory, never
extracted by member path. The branch is public data (DESIGN §1.5), so a record
carries no actor, author or e-mail.

Standard library only; the `gh` and `git` CLIs are its only tools.
"""

from __future__ import annotations

import argparse
import base64
import html
import io
import json
import os
import random
import re
import statistics
import subprocess
import sys
import tempfile
import time
import tomllib
import zipfile
from collections.abc import Callable, Iterator, Mapping
from dataclasses import dataclass, field
from datetime import UTC, date, datetime, timedelta
from pathlib import Path
from typing import Any, Protocol

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts import gatelib  # noqa: E402

SCHEMA = 1
BRANCH = "ci-history"
ROTATE_AT_BYTES = 500_000_000
PUSH_ATTEMPTS = 10
MAX_ARTIFACT_BYTES = 16 * 1024 * 1024
MAX_MEMBER_BYTES = 4 * 1024 * 1024
PER_PAGE = 100
BOT_NAME = "github-actions[bot]"
BOT_EMAIL = "41898282+github-actions[bot]@users.noreply.github.com"


@dataclass(frozen=True)
class WorkflowSpec:
    path: str
    commit_input: bool  # the workflow takes a commit as input (C-HISTORY `commit`)


# The allowlist. A record's directory name is its key, never event text; a
# later workflow is one row here and one entry in ci-history.yml's
# `workflow_run.workflows`.
WORKFLOWS: dict[str, WorkflowSpec] = {
    "ci": WorkflowSpec(".github/workflows/ci.yml", False),
    "release": WorkflowSpec(".github/workflows/release.yml", True),
}

ARCH_TOKENS = ("x86_64", "aarch64")
HISTORY_WORKFLOW = ".github/workflows/ci-history.yml"
MAIN = "main"
MAIN_EVENTS = frozenset({"push", "workflow_dispatch"})
BACKFILL_LIMIT = 200
GATE_MAP = re.compile(r"^(phase-\d+|common)\.toml$")
HEX40 = re.compile(r"[0-9a-f]{40}")
HTTP_STATUS = re.compile(r"\(HTTP (\d{3})\)")


class HistoryError(Exception):
    """The history, the API or an input is not what the tool can record."""


class NotRecorded(Exception):
    """The event names a run this tool does not record (a notice, exit 0)."""


class ApiError(HistoryError):
    def __init__(self, path: str, status: int | None, message: str) -> None:
        super().__init__(f"GET {path}: {message}")
        self.status = status


# --- API access ---------------------------------------------------------------


class Api(Protocol):
    def json(self, path: str, params: Mapping[str, str | int] | None = None) -> Any: ...

    def raw(self, path: str, max_bytes: int) -> bytes: ...


class GhApi:
    """`gh api -X GET`, argv lists only. Paging is explicit (`page`/`per_page`)."""

    def _argv(self, path: str, params: Mapping[str, str | int] | None) -> list[str]:
        argv = ["gh", "api", "-X", "GET", path.lstrip("/")]
        for k, v in (params or {}).items():
            argv += ["-f", f"{k}={v}"]
        return argv

    def json(self, path: str, params: Mapping[str, str | int] | None = None) -> Any:
        r = subprocess.run(self._argv(path, params), capture_output=True, check=False)
        if r.returncode != 0:
            err = r.stderr.decode("utf-8", "replace").strip()
            m = HTTP_STATUS.search(err)
            raise ApiError(path, int(m.group(1)) if m else None, err)
        try:
            return json.loads(r.stdout)
        except json.JSONDecodeError as e:
            raise ApiError(path, None, f"not JSON: {e}") from e

    def raw(self, path: str, max_bytes: int) -> bytes:
        p = subprocess.Popen(
            self._argv(path, None), stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        assert p.stdout is not None and p.stderr is not None
        data = p.stdout.read(max_bytes + 1)
        if len(data) > max_bytes:
            p.kill()
            p.wait()
            raise ApiError(path, None, f"more than {max_bytes} bytes")
        err = p.stderr.read().decode("utf-8", "replace").strip()
        if p.wait() != 0:
            m = HTTP_STATUS.search(err)
            raise ApiError(path, int(m.group(1)) if m else None, err)
        return data


def paged(
    api: Api, path: str, key: str, params: Mapping[str, str | int] | None = None
) -> Iterator[dict[str, Any]]:
    """Every item of a paged list endpoint whose items sit under `key`."""
    page = 1
    while True:
        body = api.json(path, {**(params or {}), "per_page": PER_PAGE, "page": page})
        items = body.get(key) if isinstance(body, dict) else body
        if not isinstance(items, list):
            raise ApiError(path, None, f"no `{key}` list")
        for item in items:
            if isinstance(item, dict):
                yield item
        if len(items) < PER_PAGE:
            return
        page += 1


# --- reading the event --------------------------------------------------------


def load_event(path: Path, repository: str) -> tuple[str, int]:
    """The workflow key and run id of a `workflow_run` event file.

    Only `workflow_run.id`, `.name`, `.path` and `repository.full_name` are
    read. A run of a workflow not in `WORKFLOWS` (name and path both), or of
    another repository, raises `NotRecorded`; a non-integer id, `HistoryError`.
    """
    try:
        event = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as e:
        raise HistoryError(f"{path}: not an event file: {e}") from e
    run = event.get("workflow_run") if isinstance(event, dict) else None
    repo = event.get("repository") if isinstance(event, dict) else None
    if not isinstance(run, dict) or not isinstance(repo, dict):
        raise HistoryError(f"{path}: not a workflow_run event")
    run_id = run.get("id")
    if not isinstance(run_id, int) or isinstance(run_id, bool) or run_id <= 0:
        raise HistoryError(f"{path}: workflow_run.id is not a positive integer")
    name, wf_path = run.get("name"), run.get("path")
    if repo.get("full_name") != repository:
        raise NotRecorded(f"run {run_id} is not a run of {repository}")
    for key, spec in WORKFLOWS.items():
        if name == key and wf_path == spec.path:
            return key, run_id
    raise NotRecorded(f"run {run_id} is not a run of a workflow ci_history.WORKFLOWS lists")


# --- building a record --------------------------------------------------------


def parse_time(v: object) -> datetime | None:
    if not isinstance(v, str) or not v:
        return None
    try:
        return datetime.fromisoformat(v.replace("Z", "+00:00"))
    except ValueError:
        return None


def seconds_between(a: object, b: object) -> int | None:
    ta, tb = parse_time(a), parse_time(b)
    if ta is None or tb is None:
        return None
    return round((tb - ta).total_seconds())


def _str(v: object) -> str | None:
    return v if isinstance(v, str) else None


def job_entry(job: Mapping[str, Any]) -> dict[str, Any]:
    """C-HISTORY's per-job fields; `seconds` is None when a time is missing."""
    steps = []
    for s in job.get("steps") or []:
        if not isinstance(s, dict):
            continue
        secs = None
        if s.get("conclusion") != "skipped":
            secs = seconds_between(s.get("started_at"), s.get("completed_at"))
        steps.append({"name": _str(s.get("name")), "seconds": secs})
    return {
        "name": _str(job.get("name")),
        "conclusion": _str(job.get("conclusion")),
        "created": _str(job.get("created_at")),
        "started": _str(job.get("started_at")),
        "completed": _str(job.get("completed_at")),
        "seconds": seconds_between(job.get("started_at"), job.get("completed_at")),
        "steps": steps,
        "runner": None,
        "results": [],
    }


def run_fields(run: Mapping[str, Any], workflow: str) -> dict[str, Any]:
    """The run-level fields of a record or a tombstone. No actor, author,
    e-mail or `head_commit` (DESIGN §1.5: the branch is public)."""
    run_id = run.get("id")
    if not isinstance(run_id, int) or isinstance(run_id, bool):
        raise HistoryError("run has no integer id")
    attempt = run.get("run_attempt")
    return {
        "schema": SCHEMA,
        "run_id": run_id,
        "workflow": workflow,
        "attempt": attempt if isinstance(attempt, int) else None,
        "event": _str(run.get("event")),
        "head_sha": _str(run.get("head_sha")),
        "branch": _str(run.get("head_branch")),
        "conclusion": _str(run.get("conclusion")),
        "created": _str(run.get("created_at")),
        "started": _str(run.get("run_started_at")),
        "finished": None,
        "runner": None,
        "jobs": [],
    }


def tombstone(run: Mapping[str, Any], workflow: str, reason: str) -> dict[str, Any]:
    """A run whose jobs the API no longer returns: its listed fields only."""
    return {**run_fields(run, workflow), "tombstone": reason}


def head_repo(run: Mapping[str, Any]) -> str | None:
    hr = run.get("head_repository")
    return _str(hr.get("full_name")) if isinstance(hr, dict) else None


def build_record(
    api: Api, repo: str, workflow: str, run_id: int, warnings: list[str] | None = None
) -> dict[str, Any]:
    """The record of run `run_id`, from the run, its latest jobs and its
    artifacts. Artifact problems are appended to `warnings`."""
    spec = WORKFLOWS[workflow]
    base = f"/repos/{repo}/actions/runs/{run_id}"
    run = api.json(base)
    if not isinstance(run, dict) or run.get("id") != run_id:
        raise HistoryError(f"run {run_id}: the API returned another object")
    if str(run.get("path", "")).split("@")[0] != spec.path:
        raise HistoryError(f"run {run_id} is not a run of {spec.path}")
    rec = run_fields(run, workflow)
    jobs = [job_entry(j) for j in paged(api, f"{base}/jobs", "jobs", {"filter": "latest"})]
    rec["jobs"] = jobs
    done = [t for t in (parse_time(j["completed"]) for j in jobs) if t is not None]
    if done:
        rec["finished"] = max(done).strftime("%Y-%m-%dT%H:%M:%SZ")
    arts = read_artifacts(api, repo, run_id)
    attach_artifacts(rec, arts)
    if spec.commit_input and arts.commit is not None:
        default = api.json(f"/repos/{repo}").get("default_branch")
        if (
            run.get("event") != "pull_request"
            and head_repo(run) == repo
            and run.get("head_branch") == default
        ):
            rec["commit"] = arts.commit
        else:
            arts.warnings.append(f"run {run_id}: commit-input ignored (not the default branch)")
    if warnings is not None:
        warnings.extend(arts.warnings)
    return rec


# --- artifacts (untrusted) ------------------------------------------------------


def slug(name: str) -> str:
    """Lowercase, each run of non-alphanumerics one `-`."""
    return re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")


def parse_runner(obj: object) -> dict[str, Any] | None:
    """`build/runner.json`, schema 1, or None."""
    if not isinstance(obj, dict) or obj.get("schema") != 1:
        return None
    out: dict[str, Any] = {}
    for k in ("os", "arch", "cpu_model"):
        v = obj.get(k)
        if not isinstance(v, str) or len(v) > 200:
            return None
        out[k] = v
    inv = obj.get("invtsc")
    if not isinstance(inv, bool):
        return None
    out["invtsc"] = inv
    return out


def parse_commit_input(obj: object) -> str | None:
    """`commit.json` (`{"schema": 1, "commit": "<40 hex>"}`), or None."""
    if not isinstance(obj, dict) or obj.get("schema") != 1:
        return None
    c = obj.get("commit")
    return c if isinstance(c, str) and HEX40.fullmatch(c) else None


@dataclass
class Artifacts:
    results: dict[str, list[dict[str, Any]]] = field(default_factory=dict)  # by <job> token
    runners: dict[str, dict[str, Any]] = field(default_factory=dict)  # by <job> token
    commit: str | None = None
    warnings: list[str] = field(default_factory=list)


def _json_members(data: bytes, label: str, warnings: list[str]) -> list[object]:
    """The parsed `*.json` members of a zip held in memory, each capped."""
    out: list[object] = []
    try:
        zf = zipfile.ZipFile(io.BytesIO(data))
        infos = zf.infolist()
    except (zipfile.BadZipFile, OSError, ValueError) as e:
        warnings.append(f"{label}: not a zip ({e})")
        return out
    for info in infos:
        if info.is_dir() or not info.filename.lower().endswith(".json"):
            continue
        if info.file_size > MAX_MEMBER_BYTES:
            warnings.append(f"{label}: a member over {MAX_MEMBER_BYTES} bytes, dropped")
            continue
        try:
            with zf.open(info) as f:
                body = f.read(MAX_MEMBER_BYTES + 1)
        except (zipfile.BadZipFile, OSError, ValueError, NotImplementedError) as e:
            warnings.append(f"{label}: an unreadable member ({e})")
            continue
        if len(body) > MAX_MEMBER_BYTES:
            warnings.append(f"{label}: a member over {MAX_MEMBER_BYTES} bytes, dropped")
            continue
        try:
            out.append(json.loads(body))
        except (UnicodeDecodeError, json.JSONDecodeError):
            warnings.append(f"{label}: a member that is not JSON, dropped")
    return out


def _results(members: list[object], label: str, warnings: list[str]) -> list[dict[str, Any]]:
    """C-RESULTS objects, through `gatelib.load_results` over fresh basenames."""
    with tempfile.TemporaryDirectory() as tmp:
        for n, obj in enumerate(members):
            Path(tmp, f"{n:04d}.json").write_text(json.dumps(obj), encoding="utf-8")
        try:
            loaded = gatelib.load_results(Path(tmp))
        except gatelib.GateError:
            warnings.append(f"{label}: not schema-1 results files, dropped")
            return []
    for d in loaded:
        d.pop("_path", None)
    return loaded


def artifact_token(name: str, prefix: str) -> str:
    """The `<job>` token of `results-<arch>-<job>` or `runner-<job>`."""
    return name[len(prefix):]


def read_artifacts(api: Api, repo: str, run_id: int) -> Artifacts:
    """The run's `results-*`, `runner-*` and `commit-input` artifacts, each at
    most `MAX_ARTIFACT_BYTES` as listed and as downloaded."""
    arts = Artifacts()
    try:
        listed = list(paged(api, f"/repos/{repo}/actions/runs/{run_id}/artifacts", "artifacts"))
    except ApiError as e:
        arts.warnings.append(f"run {run_id}: artifacts not listed (HTTP {e.status})")
        return arts
    for a in listed:
        name, aid, size = a.get("name"), a.get("id"), a.get("size_in_bytes")
        if not isinstance(name, str) or not isinstance(aid, int) or a.get("expired"):
            continue
        kind = next(
            (p for p in ("results-", "runner-") if name.startswith(p)),
            "commit-input" if name == "commit-input" else None,
        )
        if kind is None:
            continue
        label = f"artifact {aid}"
        if not isinstance(size, int) or size > MAX_ARTIFACT_BYTES:
            arts.warnings.append(f"{label}: over {MAX_ARTIFACT_BYTES} bytes, not downloaded")
            continue
        try:
            data = api.raw(f"/repos/{repo}/actions/artifacts/{aid}/zip", MAX_ARTIFACT_BYTES)
        except ApiError as e:
            arts.warnings.append(f"{label}: download failed (HTTP {e.status})")
            continue
        members = _json_members(data, label, arts.warnings)
        if kind == "results-":
            token = artifact_token(name, kind)
            arts.results.setdefault(token, []).extend(_results(members, label, arts.warnings))
        elif kind == "runner-":
            runner = next((r for r in map(parse_runner, members) if r is not None), None)
            if runner is None:
                arts.warnings.append(f"{label}: no schema-1 runner.json")
            else:
                arts.runners[artifact_token(name, kind)] = runner
        else:
            commit = next((c for c in map(parse_commit_input, members) if c is not None), None)
            if commit is None:
                arts.warnings.append(f"{label}: no schema-1 commit.json")
            arts.commit = commit
    return arts


def _token_keys(token: str) -> set[str]:
    """`x86_64-e2e-1` names `x86-64-e2e-1` and, past its arch, `e2e-1`."""
    keys = {slug(token)}
    for arch in ARCH_TOKENS:
        if token.startswith(arch + "-"):
            keys.add(slug(token[len(arch) + 1:]))
    return keys


def _job_keys(name: str) -> set[str]:
    """`tier (x86_64, e2e-1)` is `tier-x86-64-e2e-1` and, for a matrix job, its
    parenthesised values `x86-64-e2e-1`."""
    keys = {slug(name)}
    m = re.fullmatch(r"[^()]*\((.*)\)\s*", name)
    if m:
        keys.add(slug(m.group(1)))
    return keys


def match_job(jobs: list[dict[str, Any]], token: str) -> dict[str, Any] | None:
    """The one job the `<job>` token names, else the run's only job, else None."""
    want = _token_keys(token)
    hits = [j for j in jobs if isinstance(j.get("name"), str) and _job_keys(j["name"]) & want]
    if len(hits) == 1:
        return hits[0]
    if not hits and len(jobs) == 1:
        return jobs[0]
    return None


def attach_artifacts(rec: dict[str, Any], arts: Artifacts) -> None:
    """Results and runner data onto their jobs; the run-level `runner` only
    when exactly one job has one."""
    jobs: list[dict[str, Any]] = rec["jobs"]
    for token, results in sorted(arts.results.items()):
        job = match_job(jobs, token)
        if job is None:
            arts.warnings.append(f"results-{token}: no single job matches, dropped")
            continue
        job["results"].extend(results)
    for token, runner in sorted(arts.runners.items()):
        job = match_job(jobs, token)
        if job is None:
            arts.warnings.append(f"runner-{token}: no single job matches, dropped")
            continue
        job["runner"] = runner
    runners = [j["runner"] for j in jobs if j.get("runner") is not None]
    rec["runner"] = runners[0] if len(runners) == 1 else None


# --- the history repository -----------------------------------------------------


def record_path(workflow: str, run_id: int) -> str:
    if workflow not in WORKFLOWS or run_id <= 0:
        raise HistoryError(f"no record path for {workflow!r} {run_id!r}")
    return f"runs/{workflow}/{run_id}.json"


def encode(record: Mapping[str, Any]) -> bytes:
    return (json.dumps(record, indent=1, sort_keys=True) + "\n").encode("utf-8")


def valid_record(obj: object, workflow: str, run_id: int) -> bool:
    """A schema-1 record of that run: a full one or a tombstone."""
    return (
        isinstance(obj, dict)
        and obj.get("schema") == SCHEMA
        and obj.get("workflow") == workflow
        and obj.get("run_id") == run_id
        and isinstance(obj.get("jobs"), list)
        and (
            "tombstone" not in obj
            or (isinstance(obj.get("tombstone"), str) and obj["jobs"] == [])
        )
    )


def _safe_relpath(p: str) -> bool:
    parts = p.split("/")
    return bool(p) and not p.startswith("/") and all(x not in ("", ".", "..") for x in parts)


def default_workdir() -> Path:
    if os.environ.get("GITHUB_ACTIONS") == "true" and os.environ.get("RUNNER_TEMP"):
        return Path(os.environ["RUNNER_TEMP"]) / BRANCH
    return ROOT / "build" / BRANCH


def default_remote() -> str:
    repo = os.environ.get("GITHUB_REPOSITORY")
    if os.environ.get("GITHUB_ACTIONS") == "true" and repo:
        return f"https://github.com/{repo}.git"
    return gatelib.git(ROOT, "remote", "get-url", "origin").strip()


def git_env() -> dict[str, str]:
    """The environment without identity overrides, so the `-c` identity holds."""
    drop = ("GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL")
    return {k: v for k, v in os.environ.items() if k not in drop}


class HistoryRepo:
    """The `ci-history` branch in a local work tree: the one writer
    (`commit_files`) and the one reader (`records`, `has`)."""

    def __init__(
        self,
        workdir: Path,
        remote: str,
        *,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self.workdir = workdir
        self.remote = remote
        self.sleep = sleep
        self.invalid: list[str] = []  # record files that are not a valid record

    def git(self, *args: str, check: bool = True, input: bytes | None = None) -> str:
        r = subprocess.run(
            [
                "git", "-C", str(self.workdir),
                "-c", f"user.name={BOT_NAME}", "-c", f"user.email={BOT_EMAIL}",
                "-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false",
                "-c", "core.hooksPath=/dev/null", "-c", "core.quotepath=off",
                *args,
            ],
            capture_output=True,
            input=input,
            check=False,
            env=git_env(),
        )
        if check and r.returncode != 0:
            err = r.stderr.decode("utf-8", "replace").strip()
            raise HistoryError(f"git {args[0]}: {err}")
        return r.stdout.decode("utf-8", "replace")

    def _ok(self, *args: str) -> bool:
        try:
            self.git(*args)
        except HistoryError:
            return False
        return True

    def remote_tip(self) -> str | None:
        out = self.git("ls-remote", "--heads", self.remote, f"refs/heads/{BRANCH}")
        return out.split()[0] if out.strip() else None

    def open(self, depth: int | None = 1) -> None:
        """Check out `ci-history` alone (`depth` commits, or all of it), or
        start it as an orphan branch when the remote has none."""
        wd = self.workdir
        if not (wd / ".git").exists():
            if wd.exists() and any(p.name != "archives" for p in wd.iterdir()):
                raise HistoryError(f"{wd}: exists and is not a ci-history work tree")
            wd.mkdir(parents=True, exist_ok=True)
            self.git("init", "-q")
            self.git("remote", "add", "origin", self.remote)
            exclude = wd / ".git" / "info" / "exclude"
            exclude.parent.mkdir(parents=True, exist_ok=True)
            exclude.write_text("/archives/\n", encoding="utf-8")
        else:
            self.git("remote", "set-url", "origin", self.remote)
        self._fallback_credentials()
        if self.remote_tip() is None:
            self.git("symbolic-ref", "HEAD", f"refs/heads/{BRANCH}")
            self.git("read-tree", "--empty")
            self.git("clean", "-fdq")
            if self._ok("rev-parse", "--verify", "-q", f"refs/heads/{BRANCH}"):
                self.git("update-ref", "-d", f"refs/heads/{BRANCH}")
            return
        self.fetch(depth)
        self.git("checkout", "-q", "-B", BRANCH, f"refs/remotes/origin/{BRANCH}")
        self.git("reset", "-q", "--hard", f"refs/remotes/origin/{BRANCH}")
        self.git("clean", "-fdq")

    def fetch(self, depth: int | None) -> None:
        args = ["fetch", "-q", "--no-tags"]
        if depth is not None:
            args.append(f"--depth={depth}")
        elif (self.workdir / ".git" / "shallow").exists():
            args.append("--unshallow")
        self.git(*args, "origin", f"+refs/heads/{BRANCH}:refs/remotes/origin/{BRANCH}")

    def _fallback_credentials(self) -> None:
        """In CI, when `gh auth setup-git` left git no credential helper, give
        this clone the job token as an extra header, written to its local
        config file: never on argv, never printed."""
        token = os.environ.get("GH_TOKEN")
        if os.environ.get("GITHUB_ACTIONS") != "true" or not token:
            return
        if self._ok("config", "--get-urlmatch", "credential.helper", "https://github.com/"):
            return
        cred = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        with open(self.workdir / ".git" / "config", "a", encoding="utf-8") as f:
            f.write(f'[http "https://github.com/"]\n\textraheader = AUTHORIZATION: basic {cred}\n')

    def head(self) -> str | None:
        out = self.git("rev-parse", "--verify", "-q", "HEAD", check=False).strip()
        return out or None

    def _apply(self, files: Mapping[str, bytes]) -> bool:
        """Write `files` into the work tree and stage them; True when the
        index then differs from HEAD (or HEAD is unborn)."""
        for rel, data in files.items():
            p = self.workdir / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(data)
        self.git("add", "-f", "--", *files)
        if self.head() is None:
            return True
        return not self._ok("diff", "--cached", "--quiet")

    def commit_files(self, files: Mapping[str, bytes], message: str) -> str | None:
        """Commit `files` on the branch tip and push. On a rejected push, fetch
        the new tip, re-apply only these files on it (a rebase of the one
        record commit, never of a shallow history), commit and push again, up
        to `PUSH_ATTEMPTS` times. Returns the pushed commit, or None when the
        branch already held the files."""
        if not files:
            return None
        for rel in files:
            if not _safe_relpath(rel):
                raise HistoryError(f"refusing to write {rel!r}")
        for attempt in range(1, PUSH_ATTEMPTS + 1):
            if not self._apply(files):
                return None
            self.git("commit", "-q", "--no-verify", "-m", message)
            if self._ok("push", "-q", "origin", f"HEAD:refs/heads/{BRANCH}"):
                return self.head()
            if attempt == PUSH_ATTEMPTS:
                break
            self.sleep(random.uniform(1.0, 5.0))
            self.fetch(1)
            self.git("reset", "-q", "--hard", f"refs/remotes/origin/{BRANCH}")
            self.git("clean", "-fdq")
        raise HistoryError(f"push to {BRANCH} failed {PUSH_ATTEMPTS} times")

    def _branch_records(self, workflow: str | None) -> Iterator[dict[str, Any]]:
        runs = self.workdir / "runs"
        keys = [workflow] if workflow is not None else sorted(WORKFLOWS)
        for key in keys:
            d = runs / key
            if not d.is_dir():
                continue
            for p in sorted(d.glob("*.json")):
                rec = self._read(p, key)
                if rec is None:
                    self.invalid.append(str(p.relative_to(self.workdir)))
                else:
                    yield rec

    @staticmethod
    def _read(p: Path, key: str) -> dict[str, Any] | None:
        if not p.stem.isdigit():
            return None
        try:
            obj = json.loads(p.read_bytes())
        except (OSError, UnicodeDecodeError, json.JSONDecodeError):
            return None
        if not valid_record(obj, key, int(p.stem)):
            return None
        assert isinstance(obj, dict)
        return obj

    def records(self, workflow: str | None = None) -> list[dict[str, Any]]:
        """Every valid record on the branch, of one workflow or all. Files that
        are not a valid record go to `invalid` and count as missing."""
        self.invalid = []
        return list(self._branch_records(workflow))

    def has(self, workflow: str, run_id: int) -> bool:
        p = self.workdir / record_path(workflow, run_id)
        return p.is_file() and self._read(p, workflow) is not None


# --- completeness -------------------------------------------------------------


def landing_commit(api: Api, repo: str) -> tuple[str, datetime] | None:
    """The oldest commit reachable from `main` that touches the history
    workflow, and its committer date; None before the history lands."""
    oldest: dict[str, Any] | None = None
    for c in paged(api, f"/repos/{repo}/commits", "commits",
                   {"sha": MAIN, "path": HISTORY_WORKFLOW}):
        oldest = c
    if oldest is None:
        return None
    sha = oldest.get("sha")
    commit = oldest.get("commit")
    committer = commit.get("committer") if isinstance(commit, dict) else None
    when = parse_time(committer.get("date")) if isinstance(committer, dict) else None
    if not isinstance(sha, str) or when is None:
        raise HistoryError("the landing commit has no sha or date")
    return sha, when


def descends(api: Api, repo: str, base: str, sha: str) -> bool:
    """`sha` is `base` or a descendant of it (the Compare API)."""
    body = api.json(f"/repos/{repo}/compare/{base}...{sha}")
    return isinstance(body, dict) and body.get("status") in ("ahead", "identical")


def on_main(run: Mapping[str, Any], repo: str) -> bool:
    """A push or dispatch run of `main` in this repository, completed. A fork's
    pull request from its own `main` is not one."""
    return (
        run.get("head_branch") == MAIN
        and run.get("event") in MAIN_EVENTS
        and head_repo(run) == repo
        and run.get("status") == "completed"
    )


def _months(start: date, end: date) -> Iterator[tuple[date, date]]:
    """Calendar-month windows covering `start..end`, newest first."""
    first = end.replace(day=1)
    while first >= start.replace(day=1):
        nxt = (first + timedelta(days=32)).replace(day=1)
        yield max(first, start), min(nxt - timedelta(days=1), end)
        first = (first - timedelta(days=1)).replace(day=1)


def main_runs(
    api: Api,
    repo: str,
    workflow: str,
    since: datetime | None = None,
    now: datetime | None = None,
) -> Iterator[dict[str, Any]]:
    """The workflow's completed runs on `main`, newest first, listed one month
    of `created` at a time (a filtered list returns at most 1,000 runs). With
    no `since`, from the workflow's creation."""
    file = WORKFLOWS[workflow].path.rsplit("/", 1)[-1]
    base = f"/repos/{repo}/actions/workflows/{file}"
    if since is None:
        since = parse_time(api.json(base).get("created_at"))
        if since is None:
            raise HistoryError(f"{file}: no created_at")
    end = (now or datetime.now(UTC)).date()
    for lo, hi in _months(since.date(), end):
        params = {"branch": MAIN, "created": f"{lo.isoformat()}..{hi.isoformat()}"}
        for run in paged(api, f"{base}/runs", "workflow_runs", params):
            if on_main(run, repo):
                yield run


def gate_job_entries(gates_dir: Path) -> set[str]:
    """The `WORKFLOWS` keys named by `job = {workflow, job}` entries of the
    gate maps (`phase-<N>.toml`, `common.toml`; C-GATEMAP). `ci` and `ci.yml`
    name the same workflow. Empty until a gate map exists."""
    out: set[str] = set()
    if not gates_dir.is_dir():
        return out
    by_name = {k: k for k in WORKFLOWS}
    for k, spec in WORKFLOWS.items():
        by_name[spec.path.rsplit("/", 1)[-1]] = k
        by_name[spec.path] = k
    for p in sorted(gates_dir.iterdir()):
        if not GATE_MAP.match(p.name):
            continue
        try:
            data = tomllib.loads(p.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as e:
            raise HistoryError(f"{p}: {e}") from e
        lines = data.get("line", [])
        for line in lines if isinstance(lines, list) else []:
            entries = line.get("entry", []) if isinstance(line, dict) else []
            for entry in entries if isinstance(entries, list) else []:
                job = entry.get("job") if isinstance(entry, dict) else None
                wf = job.get("workflow") if isinstance(job, dict) else None
                if isinstance(wf, str):
                    key = by_name.get(wf) or by_name.get(wf.removesuffix(".yaml") + ".yml")
                    if key is not None:
                        out.add(key)
    return out


def check_complete(
    api: Api,
    repo: str,
    history: HistoryRepo,
    gated: str | None,
    workflows_needed: set[str],
    workflow: str = "ci",
    now: datetime | None = None,
) -> list[str]:
    """One line per `workflow` run on `main` since the history landed that has
    no valid record or tombstone, and per tombstoned run a gate entry needs:
    one that proves `gated` (`gatelib.run_proves_commit`) when no full record
    with conclusion `success` proves it."""
    landing = landing_commit(api, repo)
    if landing is None:
        return []
    base, landed = landing
    records = history.records(workflow)
    held = {r["run_id"]: r for r in records}
    proven = bool(gated) and any(
        "tombstone" not in r
        and r.get("conclusion") == "success"
        and gatelib.run_proves_commit(r, gated or "")
        for r in records
    )
    problems: list[str] = []
    for run in main_runs(api, repo, workflow, landed, now):
        run_id = run.get("id")
        created = parse_time(run.get("created_at"))
        if not isinstance(run_id, int) or created is None or created < landed:
            continue
        rec = held.get(run_id)
        if rec is None:
            if history.has(workflow, run_id):
                continue  # archived
            head = str(run.get("head_sha", ""))
            if HEX40.fullmatch(head) and descends(api, repo, base, head):
                problems.append(
                    f"{workflow} run {run_id} ({head[:12]}, {run.get('created_at')}) "
                    "has no record on ci-history"
                )
            continue
        if (
            "tombstone" in rec
            and workflow in workflows_needed
            and gated
            and gatelib.run_proves_commit(rec, gated)
            and not proven
        ):
            problems.append(
                f"{workflow} run {run_id} is a tombstone ({rec['tombstone']}) but a gate "
                f"entry needs it as proof of {gated[:12]}"
            )
    return problems


# --- backfill -----------------------------------------------------------------


def backfill(
    api: Api, repo: str, history: HistoryRepo, limit: int = BACKFILL_LIMIT,
    workflow: str = "ci", now: datetime | None = None,
) -> tuple[int, int]:
    """Write, in one commit, the record of each listed `workflow` run on `main`
    that neither the branch nor an archive holds, or a tombstone when its jobs
    are gone (404, 410, or none), newest first, at most `limit` of them.
    Returns (records, tombstones)."""
    files: dict[str, bytes] = {}
    written = tombs = 0
    for run in main_runs(api, repo, workflow, None, now):
        if written + tombs >= limit:
            break
        run_id = run.get("id")
        if not isinstance(run_id, int) or isinstance(run_id, bool) or run_id <= 0:
            continue
        if history.has(workflow, run_id):
            continue
        try:
            rec = build_record(api, repo, workflow, run_id)
        except ApiError as e:
            if e.status not in (404, 410):
                raise
            rec = tombstone(run, workflow, f"the jobs API returned {e.status}")
        if not rec["jobs"] and "tombstone" not in rec:
            rec = tombstone(run, workflow, "the jobs API returned no jobs")
        if "tombstone" in rec:
            tombs += 1
        else:
            written += 1
        files[record_path(workflow, run_id)] = encode(rec)
    history.commit_files(files, f"backfill {workflow}: {written} records, {tombs} tombstones")
    return written, tombs


# --- series -------------------------------------------------------------------


@dataclass(frozen=True)
class Point:
    finished: str
    head_sha: str
    run_id: int
    seconds: int


def push_to_green(rec: Mapping[str, Any]) -> int | None:
    """The latest job `completed` minus the earliest job `created`."""
    jobs = [j for j in rec.get("jobs", []) if isinstance(j, dict)]
    created = [t for t in (parse_time(j.get("created")) for j in jobs) if t is not None]
    done = [t for t in (parse_time(j.get("completed")) for j in jobs) if t is not None]
    if not created or not done:
        return None
    return round((max(done) - min(created)).total_seconds())


def _find_job(rec: Mapping[str, Any], name: str) -> dict[str, Any] | None:
    jobs = [j for j in rec.get("jobs", []) if isinstance(j, dict)]
    exact = [j for j in jobs if j.get("name") == name]
    if exact:
        return exact[0]
    loose = [j for j in jobs if isinstance(j.get("name"), str) and slug(j["name"]) == slug(name)]
    return loose[0] if len(loose) == 1 else None


def series(
    history: HistoryRepo,
    workflow: str,
    job: str | None = None,
    step: str | None = None,
    branch: str | None = MAIN,
    events: frozenset[str] | None = MAIN_EVENTS,
) -> list[Point]:
    """One point per record of `workflow` on `branch` from `events` (None or
    empty: any), oldest first: the push-to-green time, a job's seconds, or
    the seconds of a job's steps named `step`, summed."""
    out: list[Point] = []
    for rec in history.records(workflow):
        if "tombstone" in rec:
            continue
        if branch and rec.get("branch") != branch:
            continue
        if events and rec.get("event") not in events:
            continue
        value: int | None
        if job is None:
            value = push_to_green(rec)
        else:
            j = _find_job(rec, job)
            if j is None:
                continue
            if step is None:
                value = j.get("seconds")
            else:
                secs = [
                    s.get("seconds") for s in j.get("steps", [])
                    if isinstance(s, dict) and s.get("name") == step
                ]
                known = [x for x in secs if isinstance(x, int)]
                value = sum(known) if known else None
        if not isinstance(value, int) or isinstance(value, bool):
            continue
        out.append(Point(
            finished=str(rec.get("finished") or rec.get("started") or ""),
            head_sha=str(rec.get("head_sha") or ""),
            run_id=int(rec["run_id"]),
            seconds=value,
        ))
    out.sort(key=lambda p: (p.finished, p.run_id))
    return out


def print_series(points: list[Point]) -> None:
    for p in points:
        print(f"{p.finished}  {p.head_sha[:12]}  {p.run_id}  {p.seconds}")
    if points:
        med = statistics.median(p.seconds for p in points)
        print(f"median {med:g} s over {len(points)} runs")
    else:
        print("no matching records")


# --- output -------------------------------------------------------------------


def escape(text: str) -> str:
    """API text made inert in the Markdown job summary."""
    return re.sub(r"([\\`*_{}\[\]()#+!|~])", r"\\\1", html.escape(text, quote=False))


def summary(lines: list[str]) -> None:
    """Print `lines` and append them, escaped, to `$GITHUB_STEP_SUMMARY`."""
    for line in lines:
        print(line)
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a", encoding="utf-8") as f:
            for line in lines:
                f.write(escape(line) + "\n\n")


# --- main ---------------------------------------------------------------------


def _repository() -> str:
    repo = os.environ.get("GITHUB_REPOSITORY")
    if repo:
        return repo
    url = gatelib.git(ROOT, "remote", "get-url", "origin").strip()
    m = re.search(r"github\.com[:/]([^/]+/[^/]+?)(?:\.git)?/?$", url)
    if m is None:
        raise HistoryError("set GITHUB_REPOSITORY: origin is not a github.com remote")
    return m.group(1)


def local_head() -> str | None:
    """`git rev-parse HEAD` when the tool runs inside a checkout of the tree."""
    top = gatelib.git(ROOT, "rev-parse", "--show-toplevel", check=False).strip()
    if not top or Path(top).resolve() != ROOT:
        return None
    return gatelib.git(ROOT, "rev-parse", "HEAD", check=False).strip() or None


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(
        prog="ci_history.py",
        description="CI history on the ci-history branch (ROADMAP §10.9). With no mode, "
        "the completeness check: fail on a ci run on main since the history landed "
        "that has no record.",
    )
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument(
        "--event", metavar="PATH", type=Path,
        help="record the workflow_run event's run (the history workflow's record job)",
    )
    mode.add_argument(
        "--backfill", action="store_true",
        help="record listed ci runs on main the history lacks, or tombstone them",
    )
    mode.add_argument(
        "--series", metavar="WORKFLOW", choices=sorted(WORKFLOWS),
        help="print a run's push-to-green time, or a job's or step's seconds, per record",
    )
    ap.add_argument("--job", metavar="NAME", help="--series: the job")
    ap.add_argument("--step", metavar="NAME", help="--series: the job's step")
    ap.add_argument("--branch", default=MAIN, help="--series: the runs' branch ('' for any)")
    ap.add_argument(
        "--events", default=",".join(sorted(MAIN_EVENTS)),
        help="--series: the runs' events, comma-separated ('' for any)",
    )
    ap.add_argument("--limit", type=int, default=BACKFILL_LIMIT,
                    help=f"--backfill: at most N runs (default {BACKFILL_LIMIT})")
    ap.add_argument("--gated", metavar="SHA",
                    help="default mode: the gated commit (default: HEAD of this checkout)")
    ap.add_argument("--gates", metavar="DIR", type=Path,
                    help="default mode: the gate maps' directory (default tests/gates)")
    ap.add_argument("--history", metavar="DIR", type=Path, help="work tree of the branch")
    ap.add_argument("--remote", metavar="URL", help="remote holding the branch")
    return ap.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    workdir = args.history or default_workdir()
    try:
        if args.event is not None:
            repo = os.environ.get("GITHUB_REPOSITORY", "")
            try:
                workflow, run_id = load_event(args.event, repo)
            except NotRecorded as e:
                print(f"::notice::ci_history: {e}; nothing recorded")
                return 0
            api = GhApi()
            warnings: list[str] = []
            rec = build_record(api, repo, workflow, run_id, warnings)
            history = HistoryRepo(workdir, args.remote or default_remote())
            history.open(depth=1)
            sha = history.commit_files(
                {record_path(workflow, run_id): encode(rec)}, f"{workflow} run {run_id}"
            )
            summary(
                [f"ci-history: {workflow} run {run_id} ({len(rec['jobs'])} jobs), "
                 f"commit {sha or 'unchanged'}"]
                + [f"warning: {w}" for w in warnings]
            )
            return 0
        if args.series is not None:
            if args.step is not None and args.job is None:
                print("ci_history: --step needs --job", file=sys.stderr)
                return 2
            history = HistoryRepo(workdir, args.remote or default_remote())
            history.open(depth=1)
            events = frozenset(e for e in args.events.split(",") if e)
            print_series(series(history, args.series, args.job, args.step,
                                args.branch or None, events or None))
            return 0
        repo = _repository()
        api = GhApi()
        history = HistoryRepo(workdir, args.remote or default_remote())
        if args.backfill:
            history.open(depth=1)
            written, tombs = backfill(api, repo, history, args.limit)
            summary([f"ci-history backfill: {written} records, {tombs} tombstones"])
            return 0
        history.open(depth=1)
        gated = args.gated or local_head()
        needed = gate_job_entries(args.gates or gatelib.GATES)
        problems = check_complete(api, repo, history, gated, needed)
        summary([f"ci-history: {p}" for p in problems]
                or ["ci-history: every ci run on main since the history landed has a record"])
        return 1 if problems else 0
    except HistoryError as e:
        print(f"ci_history: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
