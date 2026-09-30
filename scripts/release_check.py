#!/usr/bin/env python3
"""The release gate `release.yml`'s `build` job runs first (ROADMAP §10.1).

`release.yml` runs this file from `main`'s own sparse checkout, before any
code of the tag's tree, so a tag cannot bring the checks it is judged by.
Through the GitHub API, in order:

- the tag has the form `v<major>.<minor>.<patch>`;
- `git/ref/tags/<tag>` (an exact match) is an annotated tag;
- the tag object points at a commit;
- that commit is on `main` (`compare/<commit>...main` is `ahead` or
  `identical`);
- a `ci.yml` run for that commit concluded `success` and proves it under
  `gatelib.run_proves_commit` (ROADMAP §10.9);
- exactly one annotated `phase-<N>` tag points at that commit, the phase
  whose `make gate` the job runs.

On success it prints `commit=<sha>` and `phase=<N>`, for `$GITHUB_OUTPUT`;
otherwise `release: <reason>` on stderr and exit 1. `gh` is its only tool
(`GH_TOKEN` in the environment). Standard library only.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts.gatelib import run_proves_commit  # noqa: E402

TAG = re.compile(r"v\d+\.\d+\.\d+")
PHASE_TAG = re.compile(r"phase-(\d+)")
SHA = re.compile(r"[0-9a-f]{40}")
ON_MAIN = frozenset({"ahead", "identical"})
RELEASING = "docs/RELEASING.md"

Api = Callable[[str], Any]


class ReleaseError(Exception):
    """The tag cannot be released; the message says why."""


def gh_api(path: str) -> Any:
    """`gh api <path>`, parsed; a failed call is a `ReleaseError`."""
    r = subprocess.run(["gh", "api", path], capture_output=True, text=True, check=False)
    if r.returncode != 0:
        raise ReleaseError(f"gh api {path}: {r.stderr.strip() or f'exit {r.returncode}'}")
    try:
        return json.loads(r.stdout)
    except json.JSONDecodeError as e:
        raise ReleaseError(f"gh api {path}: not JSON ({e})") from e


def _field(obj: Any, *keys: str) -> Any:
    for k in keys:
        if not isinstance(obj, dict):
            return None
        obj = obj.get(k)
    return obj


def tag_commit(api: Api, repo: str, name: str) -> str:
    """The commit the annotated tag `name` points at."""
    ref = api(f"repos/{repo}/git/ref/tags/{name}")
    if _field(ref, "ref") != f"refs/tags/{name}":
        raise ReleaseError(f"tag {name} not found")
    if _field(ref, "object", "type") != "tag":
        raise ReleaseError(f"tag {name} is not annotated (git tag -a; {RELEASING})")
    obj = api(f"repos/{repo}/git/tags/{_field(ref, 'object', 'sha')}")
    target = _field(obj, "object", "type")
    commit = _field(obj, "object", "sha")
    if target != "commit" or not isinstance(commit, str) or not SHA.fullmatch(commit):
        raise ReleaseError(f"tag {name} points at a {target}, not a commit")
    return commit


def on_main(api: Api, repo: str, commit: str) -> None:
    # ROADMAP §22.1: a `release/v<x>.<m>` branch joins `main` here.
    status = _field(api(f"repos/{repo}/compare/{commit}...main"), "status")
    if status not in ON_MAIN:
        raise ReleaseError(f"commit {commit} is not on main (compare: {status})")


def ci_passed(api: Api, repo: str, commit: str) -> None:
    runs = _field(
        api(f"repos/{repo}/actions/workflows/ci.yml/runs?head_sha={commit}&per_page=100"),
        "workflow_runs",
    )
    for run in runs if isinstance(runs, list) else []:
        if (
            isinstance(run, dict)
            and run.get("conclusion") == "success"
            and run_proves_commit(run, commit)
        ):
            return
    raise ReleaseError(
        f"no successful ci run proves {commit} (a push or dispatch run; see {RELEASING})"
    )


def phase(api: Api, repo: str, commit: str) -> int:
    """N of the one annotated `phase-<N>` tag at `commit`."""
    refs = api(f"repos/{repo}/git/matching-refs/tags/phase-")
    found = []
    for ref in refs if isinstance(refs, list) else []:
        name = str(_field(ref, "ref") or "").removeprefix("refs/tags/")
        m = PHASE_TAG.fullmatch(name)
        if m is None or _field(ref, "object", "type") != "tag":
            continue
        if tag_commit(api, repo, name) == commit:
            found.append(int(m.group(1)))
    if len(found) != 1:
        what = "no" if not found else f"{len(found)}"
        raise ReleaseError(f"{what} annotated phase-<N> tags at {commit} ({RELEASING})")
    return found[0]


def check(api: Api, repo: str, tag: str) -> str:
    """The commit `tag` releases, or `ReleaseError`."""
    if not TAG.fullmatch(tag):
        raise ReleaseError(f"tag {tag!r} is not v<major>.<minor>.<patch>")
    commit = tag_commit(api, repo, tag)
    on_main(api, repo, commit)
    ci_passed(api, repo, commit)
    return commit


def main(argv: list[str] | None = None, api: Api = gh_api) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--repo", required=True, help="owner/name")
    ap.add_argument("--tag", required=True, help="the release tag, v0.<m>.0")
    args = ap.parse_args(argv)
    try:
        commit = check(api, args.repo, args.tag)
        n = phase(api, args.repo, commit)
    except ReleaseError as e:
        print(f"release: {e}", file=sys.stderr)
        return 1
    print(f"commit={commit}")
    print(f"phase={n}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
