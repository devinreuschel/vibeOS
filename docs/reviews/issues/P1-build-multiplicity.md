# P1 · Cut the build multiplicity (5 target dirs × 2 passes × build-std)

| | |
|---|---|
| **Area** | 4.7 Performance (build) |
| **Impact / Effort / Phase** | Low / M / II |
| **Depends on** | B1 (parametrized recipes); B2 removes most of the cost |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.7](../ARCHITECTURE_REVIEW.md#47-performance--scalability) |

## Problem

`make test` builds the kernel in five `CARGO_TARGET_DIR`s (`target`, `target-panic`, `target-gp`, `target-kernel-tests`, `target-vibefs-crash`), each twice for the ksyms two-pass (`Makefile` recipes for `$(KERNEL_ELF)`, `$(ISO_PANIC)`, `$(ISO_GP)`, `$(ISO_KTEST)`, `$(ISO_VIBEFS_CRASH)`), and each rebuilding `core`, `alloc`, `compiler_builtins` from source (`-Zbuild-std`). That is ten kernel links and five standard-library builds per full run. CI caches only `target`. The maintainer reports CI is nonetheless "a few minutes", so this is about local iteration and simplicity.

## Recommended fix

Share the standard-library artifacts across variants (or eliminate them with B2), keep the "a test build can never be packaged as production" property by naming outputs, and cache everything in CI.

## Implementation plan

1. **Prefer B2.** With the built-in target, `core`/`alloc` come from rustup and the five builds only differ by the kernel crate's features; cargo fingerprints features, so one `target/` serves all variants. Steps 2–4 then reduce to renaming outputs.
2. **Single target dir, named outputs.** Drop `CARGO_TARGET_DIR=target-<variant>`; after each build copy `target/<triple>/<profile>/vibeos` to `build/kernels/vibeos-<variant>.elf` immediately, and have every ISO recipe consume only `build/kernels/…`. The DESIGN §8.2 leak scenario ("feature-enabled ELF packaged into the production ISO") is prevented by the ISO recipe never reading `target/` directly; document this in §8.2.
3. **Two-pass ksyms stays** (the second link is required for a stable `.text`); but pass 1 of every variant can skip the ksyms generation when `VIBEOS_KSYMS` is unchanged (already the case via `rerun-if-env-changed`).
4. **CI cache:** `path: target` already; ensure `build/` is not cached (outputs) and `~/.cargo/registry` is.
5. **Measure:** record `time make test` before and after on the maintainer's Mac and on CI; put the numbers in the PR.

## Acceptance criteria

- One `target/` directory; `ls target-*` empty.
- `make test` wall time on a warm cache reduced (target: at least the cost of four std builds).
- The five ISOs contain the intended ELFs (`nm` of each shows/hides `ktest`, `gp_test_trip`, `crash_loop` as expected; add this as a `make check-isos` script).

## Tests

`check-isos` script; full ladder.

## Risks and rollback

Sharing a target dir means a stale `build/kernels/*.elf` could be packaged if a build fails silently; the recipes must `rm` the output before building. Rollback: restore per-variant dirs.
