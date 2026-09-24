# B3 · Version, tag phases, and publish the ISO

**Status:** in the [index](README.md). #81: version `0.8.0`, changelog cut, `release.yml`, CI ISO artifact.
Maintainer tags `v0.8.0` later — this change does not push tags.
**Superseded in part 2026-09-22.** ROADMAP's tag rule adds an annotated `phase-<N>` tag to each phase
exit from Phase 8 on and numbers releases in closing order, since phases run side by side from Phase 15
on; `v0.8.0` to `v0.14.0` still match Phases 8 to 14, and Phase 39 is `v1.0.0`.
**Superseded in part 2026-09-23:** step 1's backfill tag at `dd04ac9` is not cut. The kernel review
reopened Phase 8's gate lines, so `v0.8.0` goes on the commit that closes them (ROADMAP, How to read this).
**Superseded in part 2026-09-24:** step 4's tag trigger. `release.yml` is dispatched from `main` with the
release tag as input, so the workflow that will hold keys is always `main`'s (ROADMAP §10.1).
**Superseded in part 2026-09-24:** step 4's `vibeos-ktest.iso`; `release.yml` publishes `vibeos.iso` alone (ROADMAP §10.1, F145).

| | |
|---|---|
| **Area** | 4.10 Release |
| **Impact / Effort / Phase** | Low / S / I |
| **Depends on** | — |
| **Blocks** | DOC4 (cutting changelog sections) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.10](../ARCHITECTURE_REVIEW.md#410-build-cicd--release-process) |

## Problem

`Cargo.toml` version is `0.0.0`; `CHANGELOG.md` has a single `[Unreleased]` section since the first commit; there are no git tags; CI does not publish the ISO. The roadmap already provides natural release points (phase exit gates); README says "follows Semantic Versioning" without any version.

## Recommended fix

Tag each phase exit as `v0.<phase>.0`, cut the changelog, and attach the production ISO to a GitHub release so a phase can be booted without a toolchain.

## Implementation plan

1. **Backfill Phase 8:** `git tag -a v0.8.0 -m "Phase 8 exit: filesystems" dd04ac9` (the last commit before Phase 9 work; #67 is a Phase 5 fix on top of Phase 8). Push the tag only when the maintainer says so.
2. **Manifests:** `version = "0.8.0"` in `Cargo.toml` and `tests/hostlib/Cargo.toml` (bump with each phase; patch releases for fixes between phases are optional).
3. **Changelog:** move today's `[Unreleased]` into `## [0.8.0] - 2026-09-19` (edit the Phase 8D entry's "Phase 9 is paused" per O1); add link references at the bottom per Keep a Changelog.
4. **`release.yml`:** on `push: tags: ['v*']`: same setup as CI, `make iso`, `make test-e2e`, then `softprops/action-gh-release` (SHA-pinned per C1) attaching `vibeos.iso` and `vibeos-ktest.iso`, body from the changelog section.
5. **CI artifact on `main`:** in `ci.yml` add `actions/upload-artifact` for `vibeos.iso` (7-day retention) so any green main can be booted.
6. **ROADMAP:** each phase's exit gate gains "tag `v0.<n>.0`"; README gets a one-line "Releases" pointer.

## Acceptance criteria

- `git tag` lists `v0.8.0`; `Cargo.toml` says `0.8.0`; changelog has a dated section.
- A tag push produces a release with the ISO attached.

## Tests

The release workflow's own `make test-e2e`.

## Risks and rollback

None; tags can be deleted before they are pushed.
