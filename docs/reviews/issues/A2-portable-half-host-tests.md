# A2 · Make the portable half portable; host tests on any host

**ROADMAP:** the §10.3 box that cites A2. Where this plan and that box differ, the box decides. Superseded here: the last acceptance line; `vibeos-core` holds no assembly and no `cfg(target_arch)` at all (ROADMAP Phase 10 exit gate), not only none that is unconditional.

| | |
|---|---|
| **Area** | 4.1 Architecture & module boundaries |
| **Impact / Effort / Phase** | High / M / II |
| **Depends on** | C1 (a pinned nightly the tree builds with); B2 (workspace) |
| **Blocks** | I1, DX1 (macOS `make check`), T4, A1 |
| **Review** | [ARCHITECTURE_REVIEW.md §4.1](../ARCHITECTURE_REVIEW.md#41-architecture--module-boundaries) |
| **Status** | In the [index](README.md). #88: portable crate is workspace member `vibeos-core`; host tests use the host triple. Nested hostlib cargo config and Linux pin deleted. I1 (macOS CI job) stays parked. |

## Problem

- `tests/hostlib/.cargo/config.toml` pins `target = "x86_64-unknown-linux-gnu"`; `Makefile` `HOST_TRIPLE` hard-codes the same. On the maintainer's Apple Silicon Mac (a supported dev host) `make test-unit` fails: "the `x86_64-unknown-linux-gnu` target may not be installed".
- Building natively (`cargo test --lib --target aarch64-apple-darwin`) with the installed nightly (2026-07-31) fails first on `src/pci.rs:733 FakeBus::slot_mut` (E0499, the classic "conditional return of a borrow" case; the newer nightly CI uses accepts it through the next-generation borrow checker). Once past that it would fail to link `vibeos_switch_context`: `src/thread.rs:219` and `:263` define it in `global_asm!` under `#[cfg(target_arch = "x86_64")]`, but the `unsafe extern "C"` declaration (line 318), `switch_context` (line 334) and the `switch_context_roundtrip` test are unconditional.
- `src/dma.rs:180–195` fences are already `cfg`-gated with an atomic-fence fallback; fine as is.
- `tests/hostlib/src/lib.rs` re-lists all 41 modules by hand with `#[path]`; a mismatch with `src/lib.rs` is silent.

## Recommended fix

The kernel is x86_64-only; the portable half must still compile and test on any host, with x86-specific pieces gated. Remove the Linux pin, gate the two x86 sites, make the hostlib a real dependency (after B2), and add a macOS job (I1).

## Implementation plan

1. **Remove the borrow-checker dependency** in `src/pci.rs::FakeBus::slot_mut` (test code): find the index with `iter().position(...)`, then borrow once. This lets the tree build on the pinned nightly (C1) and on any host.
2. **Gate the context switch.** Wrap the `unsafe extern "C"` block, `switch_context`, and `switch_context_roundtrip` in `#[cfg(target_arch = "x86_64")]`. Longer term (A1) move `switch_asm` + `switch_context` to `crates/kernel/src/arch/x86_64/switch.rs`, keeping only `CpuContext`, its offsets and `prepare_thread` in core; keep the roundtrip test as an x86_64-only hostlib test (it is valuable).
3. **Remove the target pin.** Delete `tests/hostlib/.cargo/config.toml`. Before B2 the parent `.cargo/config.toml` still sets the kernel target, so hostlib must override: set `HOST_TRIPLE := $(shell rustc -vV | sed -n 's/^host: //p')` in the Makefile and pass `--target $(HOST_TRIPLE)` to hostlib builds; `MKFS_VIBEFS`/`FSCK_VIBEFS` paths become `$(CARGO_TARGET_DIR)/$(HOST_TRIPLE)/debug/…`. After B2 (no parent build-std) drop `--target` and use `$(CARGO_TARGET_DIR)/debug/…`.
4. **Guard the module list.** Add a hostlib test that reads `../../src/lib.rs`, extracts `pub mod x;` names, and asserts they equal the `#[path]` set in `tests/hostlib/src/lib.rs` (until step 5 makes it moot).
5. **After B2: workspace.** Root `Cargo.toml` `[workspace] members = ["crates/core", "crates/kernel", "tests/hostlib"]`; hostlib depends on `vibeos-core = { path = "../../crates/core", features = ["std"] }` and deletes the `#[path]` list; core gets `#![cfg_attr(not(feature = "std"), no_std)]`.
6. **Run the whole host tier on macOS** (`make test-unit`, `make test-harness`, `make test-vibefs-crash`) and fix platform-specifics, e.g. the `src/fat.rs:2094` test that panics when `fsck.fat` is missing should skip with a printed reason when `dosfstools` is absent.
7. **CI:** add the macOS job (I1).

## Acceptance criteria

- `make test-unit` passes on macOS arm64 and Ubuntu x86_64 with the pinned nightly.
- `grep -rn 'x86_64-unknown-linux-gnu' Makefile tests` is empty.
- `make test-vibefs-crash` runs on macOS (host `mkfs-vibefs`/`fsck-vibefs` found).
- No unconditional x86 asm or extern symbols remain in the portable half.

## Tests

Existing 367 host tests; the new module-list guard; the x86-only context-switch test still runs on Linux x86_64 CI.

## Risks and rollback

Step 3 changes where host binaries land; the Makefile and `tests/harness/run_vibefs_crash.py` (via `VIBEOS_MKFS`) are the only consumers. Rollback: restore the config file.

## Out of scope

Making the kernel itself build for another architecture (ROADMAP §10.3 and Phase 11).
