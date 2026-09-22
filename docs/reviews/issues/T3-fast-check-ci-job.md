# T3 · A fast `check` CI job ahead of the QEMU ladder; coverage floor

| | |
|---|---|
| **Area** | 4.6 Testing / 4.10 CI |
| **Impact / Effort / Phase** | Medium / S / II |
| **Depends on** | Q1 (`make check`), T2 |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.6](../ARCHITECTURE_REVIEW.md#46-testing-strategy--coverage) |

## Problem

`.github/workflows/ci.yml` is one job: five kernel builds then twelve QEMU steps, no fmt/clippy/coverage. The maintainer reports the whole ladder takes "a few minutes" and that GitHub runner queues are sometimes full, so fanning out into many jobs would not help and might hurt. What is missing is failing fast on cheap checks, and DESIGN §8.6's promised `cargo-llvm-cov` floor on the portable half.

## Recommended fix

Two jobs: `check` (about a minute) and the existing ladder gated on it. Coverage measured on hostlib with a floor that only ratchets up. No matrix.

## Implementation plan

1. **`check` job** (ubuntu, plus macOS per I1): checkout, pinned toolchain (C1), `./setup.sh` is not needed (no QEMU), `make check` (fmt, clippy, host unit, harness unit, cycle/size/module-map scripts as they land).
2. **Ladder job** `needs: check`; unchanged steps otherwise. Keep `concurrency` cancellation.
3. **Coverage:** in `check`, `cargo install cargo-llvm-cov --locked` (cache `~/.cargo/bin`), then `cd tests/hostlib && cargo llvm-cov --lib --fail-under-lines <N>` with `N` = current line coverage rounded down (measure once locally; record `N` in the workflow with a comment "ratchet only upward"). Upload the HTML report as an artifact.
4. **Timing visibility:** add `time` to the ladder steps or use the workflow's built-in durations; note the total in the README badge line or DESIGN §8.6 so the "few minutes" claim stays measured.
5. **Docs:** DESIGN §8.6 table of jobs.

## Acceptance criteria

- A PR that fails `cargo fmt --check` is red within about a minute without starting QEMU.
- Coverage floor enforced; report artifact present.
- Total CI wall time not worse than today by more than the `check` job's duration.

## Tests

None; CI is the test.

## Risks and rollback

`cargo llvm-cov` on hostlib compiles with instrumentation; if it doubles `check` time, run coverage only on `main` pushes.
