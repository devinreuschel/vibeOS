# Q1 · Land the promised gates: rustfmt, clippy, `-D warnings`

**ROADMAP:** the open §10.1 boxes that cite Q1. Where this plan and those boxes differ, the boxes decide. Superseded here: step 3's one rustflags table; `-D warnings` also goes in `[target.x86_64-unknown-none].rustflags`, which shadows `[build].rustflags` for the kernel. Step 2's clippy over `--all-features`; CI lints each feature set an image is built with, and `kernel_shell`, instead (ROADMAP §10.1).

| | |
|---|---|
| **Area** | 4.2 Code quality & consistency |
| **Impact / Effort / Phase** | High / S / I |
| **Depends on** | — |
| **Blocks** | A1 (format before moving), Q2, DX1, T3 |
| **Review** | [ARCHITECTURE_REVIEW.md §4.2](../ARCHITECTURE_REVIEW.md#42-code-quality--consistency) |

## Problem

- `docs/ROADMAP.md:77`: `- [ ] RUSTFLAGS=-Dwarnings, cargo clippy -- -D warnings, cargo fmt --check (deferred: clippy/fmt gates land with phase 1)` is still open at Phase 8.
- `rustfmt --edition 2024 --check src/lib.rs src/main.rs` reports diffs in 65 of 84 files. No `rustfmt.toml`, no clippy invocation in `Makefile` or CI, no `-D warnings` anywhere.
- Clippy has been run ad hoc (`#![allow(clippy::identity_op)]` in `src/paging.rs:34` and `src/pmm.rs:33`; `#[allow(clippy::mut_from_ref)]` in `src/kva_init.rs:24`) but never gated. 40+ `#[allow(dead_code)]`.
- 90 `unsafe fn` versus 50 `# Safety` sections; clippy's default `missing_safety_doc` would flag the gap.

## Recommended fix

Three commits: (1) `rustfmt.toml` + `cargo fmt` (whitespace only), (2) clippy fixes and safety docs, (3) the gates in `make check` and CI. Keep the blanket `dead_code` allows until Q2 removes their cause.

## Implementation plan

1. **`rustfmt.toml`** at the root containing `edition = "2024"` (defaults otherwise; do not fight rustfmt). Run `cargo fmt` for the kernel package and `cd tests/hostlib && cargo fmt`. Commit alone as "fmt: apply rustfmt (no code change)". Check that `git diff -w --stat` is near-empty.
2. **Clippy baseline.** `$(CARGO) clippy --lib --bin vibeos --all-features -- -D warnings` (build-std makes clippy work on the kernel target) and `cd tests/hostlib && cargo clippy --all-targets -- -D warnings`. Fix findings; where a lint is wrong for kernel code, add `#[allow(clippy::x)]` at the site with a one-line reason, never crate-wide. Add `# Safety` sections to the 40 undocumented `unsafe fn`s (one sentence each).
3. **Warnings denied.** Add `"-D", "warnings"` to `rustflags` in `.cargo/config.toml` (kernel) and `tests/hostlib/.cargo/config.toml` (or the workspace config after A2); CI's `check` job also sets `RUSTFLAGS=-D warnings` so the intent is visible.
4. **`make check`** target: `cargo fmt --check` for both packages, both clippy commands, `make test-unit`, `make test-harness`. The CI `check` job runs it (T3).
5. **Pre-existing allows.** Keep `#![allow(clippy::identity_op)]` (arithmetic clarity in paging/pmm) with a comment; remove any allow that no longer fires.
6. **Docs:** tick ROADMAP §0.7; add "run `make check` before `make test`" to `AGENTS.md` (DOC3).

## Acceptance criteria

- `cargo fmt --check` clean in both packages; CI `check` green.
- `grep -rn '#\[allow(' src | wc -l` ≤ 15, each with a reason comment.
- `unsafe fn` count equals `# Safety` section count (enforced by clippy `missing_safety_doc` under `-D warnings`).

## Tests

None new; the gates are the test.

## Risks and rollback

The fmt commit conflicts with every open branch once. Merge it when no branch is open, or run `cargo fmt` on each branch before rebasing.

## Out of scope

Restriction lints for the portable crate (E1); removing `kernel_tests` dead-code allows (Q2).
