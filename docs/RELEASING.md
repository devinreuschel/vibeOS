# Releasing vibeOS

The maintainer's steps to cut a `v0.<m>.0` release (ROADMAP §10.1, How to read this). Agents never
tag, dispatch `release.yml`, or approve a deployment ([AGENTS.md](../AGENTS.md), Identity). A release
is published only by `.github/workflows/release.yml`, dispatched from `main` with the release tag; its
`build` and `publish` jobs, and the rules `scripts/check_workflows.py` keeps them to, are in
[TESTING.md §8.6](TESTING.md#86-ci-and-coverage).

Phase `<N>` releases as `v0.<m>.0`, numbered in closing order: `v0.8.0` to `v0.14.0` are Phases 8 to
14. Below, C is the commit that closes the phase.

1. **Preconditions.**
   - Phase `<N>`'s exit gate is closed on C, and C is on `main`.
   - The phases before it are tagged, in order: Phases 8 to 14 are tagged in closing order, and
     Phase 8 also needs the gate lines of Phases 0 to 7 (ROADMAP, How to read this, standing gates).
   - The repository ruleset is applied as [Repository rulesets](#repository-rulesets) says, and
     `python3 scripts/check_gate_inputs.py --rulesets`, run with your `gh` login, prints ok.
   - `CHANGELOG.md` at C has a `## [0.<m>.0]` section. `release.yml` publishes that section as the
     release notes and fails before any upload without it.
   - The gate passes on a clean checkout of C:

         git switch --detach C
         make gate PHASE=<N>

     For Phase 8, also run `make gate PHASE=0` up to `make gate PHASE=7`. When the gate map has
     `record` entries, run `make gate PHASE=<N> RECORD=1` on the Apple Silicon Mac first, so their
     dev-host records exist. For a `job` entry with no proving run, push C to `gate/<N>` and run that
     workflow there (`gh workflow run <wf> --ref gate/<N>`); for a workflow that takes a commit as
     input, dispatch it on `main` with C.

2. **A green `ci` run proving C.** Normally C's `push` run on `main`. Otherwise:

       git push origin C:refs/heads/gate/<N>
       gh workflow run ci.yml --ref gate/<N>

   A `pull_request` run never counts (ROADMAP §10.9). Tag `main`'s current head, and dispatch before
   anything else merges: a dispatched `release.yml` run's own head is `main`'s head, and the
   `ci-history` record names C only through the run's `commit-input` artifact.

3. **Annotated tags.** Both on C, both annotated (`release.yml` refuses a lightweight tag):

       git tag -a phase-<N> -m "Phase <N>: <name>" C
       git tag -a v0.<m>.0 -m "vibeOS 0.<m>.0 (Phase <N>)" C
       git push origin phase-<N> v0.<m>.0

4. **Dispatch** from `main`:

       gh workflow run release.yml --ref main -f tag=v0.<m>.0
       gh run watch

   Dispatch from `main` only. A dispatch runs the dispatched ref's copy of the workflow: `build`
   refuses any ref but `refs/heads/main`, but only a copy that carries that guard does, until ROADMAP
   §14.6's `release` environment admits `main` alone.

5. **What the run checks and publishes.**
   - `build` (`contents: read`, `actions: read`) runs `main`'s own `scripts/release_check.py` before
     any code of the tag's tree: the tag has the form `v<x>.<y>.<z>` and is annotated, it points at a
     commit on `main`, a `ci` run for that commit concluded `success` and proves it
     (`gatelib.run_proves_commit`), and one annotated `phase-<N>` tag sits on it. It then checks the
     commit out without a persisted token, restores no cache, clones Limine and builds its host tool
     fresh (`./setup.sh`), runs `make gate PHASE=<N>`, builds the release-profile image with
     `make release-artifacts OUT=dist`, adds the third-party notices and the xorriso version, runs
     the e2e targets that boot the production image on it with `CARGO_PROFILE=release`, checks the
     tested ISO is byte-identical to `dist/vibeos.iso`, writes the notes
     (`scripts/changelog_section.py`), and uploads `dist/` with its `SHA256SUMS` as the `release`
     artifact.
   - `publish` (`contents: write`) checks out nothing and runs no repository script: it downloads
     the `release` artifact, runs `sha256sum -c SHA256SUMS`, and creates the GitHub Release at the
     verified commit with `vibeos.iso` as its one image, beside `vibeos.iso.xorriso-version` and
     `THIRD-PARTY-NOTICES.txt`.

   To check a published asset against the run:

       gh run download <run-id> -n release -D release-files
       gh release download v0.<m>.0 -p vibeos.iso -D published
       (cd release-files && sha256sum -c SHA256SUMS)
       cmp published/vibeos.iso release-files/vibeos.iso

6. **On failure** nothing is published: `publish` runs only after `build` succeeds. Fix the cause on
   `main` and dispatch again. Move a tag only while no Release uses it.

7. **Never tag a commit older than this workflow.** A `v*` tag on a commit whose `release.yml` still
   has the `push: tags` trigger runs that old file on the tag push, which publishes
   `vibeos-ktest.iso` with a write token and no `ci` check (F145).

8. **Later.** ROADMAP §14.6 adds signing here, with a `sign` job in the `release` environment between
   `build` and `publish`. The branch ruleset that protects `main` and the release branches is
   below.

## Repository rulesets

ROADMAP §10.9: `main`, and each release branch from the first one ROADMAP §22.1 cuts, requires a pull
request and the `check` and `ci-pass` checks, and blocks force pushes and deletion, with no bypass
actor. The owner applies this one branch ruleset, once, and nobody else changes it; agents never do
([AGENTS.md](../AGENTS.md), Identity). `ci-pass` is the `ci.yml` job that needs every other job, so
regrouping the CI tiers never changes the ruleset ([TESTING.md §8.6](TESTING.md#86-ci-and-coverage)).

In Settings, Rules, Rulesets, New branch ruleset:

- Name `main and release branches`, Enforcement status Active, and an empty bypass list.
- Target branches: the default branch, and the pattern `refs/heads/release/v*`.
- Only these rules: Restrict deletions; Require a pull request before merging (0 required approvals
  for a sole maintainer); Require status checks to pass, with `check` and `ci-pass` from GitHub
  Actions; Block force pushes.

The same through the API:

    gh api --method POST 'repos/{owner}/{repo}/rulesets' --input - <<'JSON'
    {
      "name": "main and release branches",
      "target": "branch",
      "enforcement": "active",
      "bypass_actors": [],
      "conditions": {
        "ref_name": {"include": ["~DEFAULT_BRANCH", "refs/heads/release/v*"], "exclude": []}
      },
      "rules": [
        {"type": "deletion"},
        {"type": "non_fast_forward"},
        {"type": "pull_request", "parameters": {
          "required_approving_review_count": 0,
          "dismiss_stale_reviews_on_push": false,
          "require_code_owner_review": false,
          "require_last_push_approval": false,
          "required_review_thread_resolution": false}},
        {"type": "required_status_checks", "parameters": {
          "strict_required_status_checks_policy": false,
          "required_status_checks": [
            {"context": "check", "integration_id": 15368},
            {"context": "ci-pass", "integration_id": 15368}]}}
      ]
    }
    JSON

Last, check the live rules with your own `gh` login, since only a ruleset admin can read
`bypass_actors`:

    python3 scripts/check_gate_inputs.py --rulesets

It reads the rules active on `main` and on each `release/v*` branch and prints
`check_gate_inputs: rulesets ok`, or names each difference. It is the proof of ROADMAP §10.9's
rulesets box, recorded as a dev-host record (`make gate PHASE=10 RECORD=1`).
