# E1 · Lock in parser robustness with restriction lints

**Status:** implemented (#86). Nine production `unwrap`/`expect` sites return errors or
cannot-fail rewrites. Clippy `unwrap_used` / `expect_used` / `panic` deny on the
portable crate. Step 2's `indexing_slicing` and `arithmetic_side_effects` levels were never set;
ROADMAP §10.1 denies both in the byte parsers, one module per PR.

| | |
|---|---|
| **Area** | 4.4 Error handling, logging & observability |
| **Impact / Effort / Phase** | Low / S / II |
| **Depends on** | Q1 (clippy gate) |
| **Blocks** | — (pairs with T4) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.4](../ARCHITECTURE_REVIEW.md#44-error-handling-logging--observability) |

## Problem (corrected in review revision 2)

Counting only non-test code (lines before each module's first `#[cfg(test)]`), the portable half contains **zero** `panic!` calls and only nine `unwrap()` / `expect()` calls: `src/kva.rs` 3, `src/vibefs.rs` 2, `src/block.rs` 1, `src/heap.rs` 1, `src/wait.rs` 1, `src/fs/kernfs.rs` 1. Every parser (`acpi::parse_*`, `part::parse_image`, `fat::FatVol::mount`, `vibefs::{probe, mount, fsck}`, `pci::read_function`, `shell::tokenize`) returns a `Result`. Nothing enforces this, so one new `unwrap()` on disk data would go unnoticed until it halts a machine (DESIGN §2.5: panics halt).

## Recommended fix

Make the discipline mechanical: clippy restriction lints scoped to the portable crate, allowed inside tests, plus fuzzing (T4) for what lints cannot see.

## Implementation plan

1. **Fix the nine sites.** For each, either prove it cannot fail and rewrite as `match` / `unwrap_or` / `let-else` with an error return, or return the module error. Expected: `kva.rs` free-list invariants (return `Err`), `vibefs.rs` (checksum/slot lookups → `Error::Corrupt`), `block.rs`, `heap.rs`, `wait.rs`, `kernfs.rs` similar.
2. **Lints.** In `src/lib.rs` (`crates/core` after A2):
   `unwrap_used = "deny"`, `expect_used = "deny"`, `panic = "deny"`, `indexing_slicing = "warn"` (start as warn; slice indexing is pervasive and mostly bounds-checked by construction), `arithmetic_side_effects = "warn"` for the parser modules only (`acpi`, `part`, `fat`, `vibefs`, `pci`).
   Test modules: `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]`.
3. **Triage `indexing_slicing`** warnings in the five parser modules: convert to `.get(..)` with `?` where the index comes from disk/firmware data; allow with a comment where it comes from a constant.
4. **Document** the rule in DESIGN §2.5: "the portable half never panics on data; it returns the module error".

## Acceptance criteria

- `cargo clippy` on the portable crate with the lints above passes under `-D warnings`.
- Non-test `unwrap` / `expect` / `panic!` count in the portable half is 0.

## Tests

Existing host tests; T4 fuzz targets exercise the same paths.

## Risks and rollback

`indexing_slicing` may be noisy; keep it at `warn` until the parser modules are clean, then promote to `deny` for those modules only.
