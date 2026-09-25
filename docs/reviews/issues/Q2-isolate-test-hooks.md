# Q2 · Isolate `kernel_tests` scaffolding from production modules

**ROADMAP:** the §10.2 box that cites Q2. Where this plan and that box differ, the box decides. Superseded here: the acceptance's count of 103 tests; the PR leaves the in-guest test count unchanged.

| | |
|---|---|
| **Area** | 4.2 Code quality & consistency |
| **Impact / Effort / Phase** | Medium / M / II |
| **Depends on** | Q1 |
| **Blocks** | T1 |
| **Review** | [ARCHITECTURE_REVIEW.md §4.2](../ARCHITECTURE_REVIEW.md#42-code-quality--consistency) |

## Problem

- 30 non-test source files reference `feature = "kernel_tests"` (83 sites); 27 modules carry a file-level `#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]`.
- Those modules export functions only `src/ktest.rs` calls: `block_init::{inject_io_fails, reset, device}`, `apic_init::{gsi_masked, timer_mode, owns_tick, timer_fires, cpuid_has_tsc_deadline, is_ready}`, `kbd_init::{push_for_test, inject_scancode, read_cfg}`, `smp_init::{exercise_fail_cleanup, trampoline_installed}`, `ipi_init::{reschedule_count, shootdown_count, call_count}`, `time_init::{measure_pit_ch2, measure_hpet, source, tsc_invariant}`, `shell_init::{command_count, has_command, dispatch_line, builtin_names}`, `pci_init::{cfg_read32, cfg_write32}`, `console_init::{set_enabled, enabled}`, `fb_init::{put_pixel, get_pixel, pitch, width, cursor}`, `irq_init::{allocated_count, has_threaded}`, `acpi_init::mmio_uc_patched`, `fat_init::nvol`, `vibefs_init::{nvol, snapshot}`, `dev_init::{len, find_id, claim}`, `file_init::{seek, symlink_path, link_path, truncate_path}`.
- The blanket `allow(dead_code)` also hides genuinely dead code, and fault-injection hooks (`inject_io_fails`) ship inside the production driver module.

## Recommended fix

Each subsystem gets a `#[cfg(feature = "kernel_tests")] pub mod testing` (before A1: inside the same file; after A1: `<subsystem>/ktest.rs`) that owns its hooks and its in-guest tests. Production code compiles identically with or without the feature. `src/ktest.rs` becomes an aggregator (T1).

## Implementation plan

1. **Classify every gated item** into (a) real API also used by the shell or another module (`fs_init::with`, `work_init::enqueue`, `file_init::seek`, …): keep public, drop the attribute; (b) observation hooks (counters, `live()`, `timer_mode()`): move into `testing`; (c) mutation hooks (`inject_io_fails`, `reset`, `push_for_test`, `inject_scancode`, `exercise_fail_cleanup`): move into `testing`, feature-only.
2. **Per module** add `#[cfg(feature = "kernel_tests")] pub mod testing { use super::*; … }` holding (b) and (c), and delete the file-level `cfg_attr(..., allow(dead_code))`. Fix any real dead code that surfaces.
3. **Move test bodies.** For each subsystem, cut its `test_*` functions from `src/ktest.rs` into its `testing` module (they only use that module's hooks plus shared helpers). Shared helpers (`catch_fault`, `free_frames`, `alloc_frame`, `Outcome`, `TestFn`) live in `src/ktest/mod.rs` (T1).
4. **Registry.** `testing::TESTS: &[(&str, TestFn)]` per module; `ktest::run` iterates over a static list of module registries.
5. **Verify nothing test-only reaches production.** `nm <production ELF> | grep -E 'inject_|push_for_test|exercise_fail'` must be empty; add as a script step in `make check` once B1 stabilizes paths.

## Acceptance criteria

- `grep -rn 'cfg_attr(not(feature = "kernel_tests"), allow(dead_code))' src` is empty.
- `grep -rn '#\[allow(dead_code)\]' src | wc -l` ≤ 5, each with a comment.
- The production ELF has no test-only symbols; `make test-kernel` and `-smp4` pass with the same 103 tests.

## Tests

Existing in-guest tests are the coverage; add `registry_complete` asserting every module's `TESTS` is non-empty.

## Risks and rollback

Mechanical; one subsystem per commit. If production code needs a `testing` hook, that hook is real API: promote it instead.

## Out of scope

Splitting `ktest.rs` into files beyond what step 3 forces (T1).
