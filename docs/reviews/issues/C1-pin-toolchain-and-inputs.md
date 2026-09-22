# C1 · Pin every external input: nightly date, action SHAs, Limine commit

| | |
|---|---|
| **Area** | 4.5 Configuration |
| **Impact / Effort / Phase** | High / S / I |
| **Depends on** | — |
| **Blocks** | A2 (needs a nightly the tree builds with), C-everything reproducible |
| **Review** | [ARCHITECTURE_REVIEW.md §4.5](../ARCHITECTURE_REVIEW.md#45-configuration--secrets-management) |

## Problem

- `rust-toolchain.toml`: `channel = "nightly"` (undated). CI: `dtolnay/rust-toolchain@nightly`. Every build resolves to that morning's nightly; the tree uses `#![feature(abi_x86_interrupt)]`, `alloc_error_handler`, `-Zbuild-std`, `-Zjson-target-spec`.
- Concrete symptom found during the review: the maintainer's installed nightly (`2026-07-31`) rejects `src/pci.rs:733` (E0499) that the newer nightly CI uses accepts through the next-generation borrow checker. The tree therefore already depends on an unrecorded toolchain date.
- `setup.sh` clones Limine at tag `v9.6.7-binary` with no commit or hash check. Workflows use `actions/checkout@v4`, `actions/cache@v4` (mutable tags). The cargo cache key hashes `**/Cargo.toml`, not `Cargo.lock` or the toolchain file.

## Recommended fix

Pin to a date known to build the tree (the last green CI run's nightly, or after A2 step 1, `nightly-2026-07-31` works too), pin actions to commit SHAs, record the Limine commit, and key caches on the lockfile plus toolchain. Keep a scheduled "latest nightly" job that may fail without blocking.

## Implementation plan

1. **Find the date.** Open the last green `ci` run for `main` (#67, 2026-09-19) and read the `install rust nightly` step output (`rustc 1.xx.0-nightly (… 2026-09-1x)`). Call it `D`. Verify locally: `rustup toolchain install nightly-D --component rust-src llvm-tools && make test-unit && make iso`.
2. **`rust-toolchain.toml`:** `channel = "nightly-D"` (components unchanged).
3. **CI:** replace `dtolnay/rust-toolchain@nightly` with `dtolnay/rust-toolchain@<commit-sha>` and `with: toolchain: nightly-D, components: rust-src, llvm-tools` (or omit `toolchain` and let the action read `rust-toolchain.toml`; check the action's docs for the pinned SHA's behaviour). Pin `actions/checkout` and `actions/cache` to SHAs with a `# vX.Y.Z` comment. Same in `smp-stress.yml`.
4. **Cache key:** `${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock', 'rust-toolchain.toml') }}`.
5. **Limine:** in `setup.sh` add `LIMINE_COMMIT="<sha of v9.6.7-binary>"`; after clone, `test "$(git -C "$LIMINE_DIR" rev-parse HEAD)" = "$LIMINE_COMMIT"` or fail. Keep the CI cache key `limine-v9.6.7-binary-<sha>`.
6. **Nightly canary:** in `smp-stress.yml` (weekly) add a job with `toolchain: nightly` and `continue-on-error: true` that runs `make iso && make test-unit`, so a breaking nightly is noticed before the pin is bumped.
7. **Bump procedure** in `AGENTS.md`: "bump the date in `rust-toolchain.toml` and CI in one PR; `make test` must be green".
8. **Dev machines:** `setup.sh` prints the pinned date and runs `rustup toolchain install` for it if missing.

## Acceptance criteria

- `grep -n 'nightly"' rust-toolchain.toml .github/workflows/*.yml` is empty (only dated nightlies).
- `grep -n '@v[0-9]' .github/workflows/*.yml` is empty (SHAs only).
- A fresh clone on macOS and Linux builds with `./setup.sh && make test-unit`.

## Tests

CI green on the pinned date; canary job exists.

## Risks and rollback

A too-old date would reject `pci.rs:733` until A2 step 1 lands; pick the CI date, not the local one, unless that fix is in.
