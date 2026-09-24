# T1 · Split the in-guest registry; make failures self-diagnosing

**ROADMAP:** the §10.2 box that cites T1. Where this plan and that box differ, the box decides. Superseded here: step 1's lists in `src/ktest/<subsystem>.rs`; each subsystem's list lives in its `ktest.rs` (§10.2's Q2 box), and `src/ktest/mod.rs` holds the runner. Step 2's timing with IRQs off; the registry runs with IF=1 (§10.2's F075 box) and times with the §10.3 cycle counter. Step 3's `alloc::format!`; a failure message formats into a fixed `StackBuf`, since §10.4 denies `format!`. Step 5; the deadline is in the guest, as the T1 box says. The acceptance's count of 103 tests; the PR leaves the count unchanged.

| | |
|---|---|
| **Area** | 4.6 Testing |
| **Impact / Effort / Phase** | Medium / M / II |
| **Depends on** | Q2 |
| **Blocks** | Q5 (ktest part) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.6](../ARCHITECTURE_REVIEW.md#46-testing-strategy--coverage) |

## Problem

- `src/ktest.rs` is one 4,083-line file with a flat `TESTS: &[(&str, TestFn)]` of 103 entries (line 73) and a runner (`run()`, line 179) that prints only after a test returns.
- `Outcome::Fail(&'static str)` carries a static reason; a hung test is a 90-second harness timeout with no name attached (the harness sees `ktest: begin` and nothing else).
- DESIGN §8.2: "When a test fails, print enough to diagnose it without a rerun."

## Recommended fix

One registry per subsystem, a `run <name>` line before each test, dynamic failure messages, per-test wall time, and harness support for "hung in `<name>`".

## Implementation plan

1. **Layout:** `src/ktest/mod.rs` (`Outcome`, `TestFn`, `run`, `qemu_exit`, shared helpers `catch_fault`, `free_frames`, `alloc_frame`, `free_frame`, `Fault`) and `src/ktest/{mm,traps,time,sched,sync,smp,log,console,pci,irq,dma,work,virtio,block,fs}.rs`, each `pub const TESTS: &[(&str, TestFn)]`. After Q2, these files are the per-subsystem `testing` modules' test lists; `mod.rs` just aggregates: `const GROUPS: &[&[(&str, TestFn)]] = &[mm::TESTS, traps::TESTS, …]`.
2. **Runner protocol:** print `vibeOS: ktest: run <name>` before calling; after, `ok <name> (<ms> ms)`, `FAIL <name>: <why>`, or `skip <name>: <reason>`. Time via `time_init::now_us()` (IRQs are off in the runner; `now_us` interpolates from TSC, fine).
3. **`Outcome::Fail(String)`** using `alloc::format!` (alloc is available); keep `Skip(&'static str)`. Update every `Outcome::Fail("…")` to `fail!("…{}", x)` via a small macro.
4. **Harness:** `check_ktest_output` records the last `run` line; on timeout or missing `end`, raise `HarnessError("ktest hung in <name>")`. Parse `(<ms> ms)` and print the ten slowest tests in the summary. Unit tests in `test_harness.py` for both.
5. **Optional watchdog:** since IRQs are off in the runner, a LAPIC one-shot cannot fire; instead, tests that spawn threads already use deadlines. Leave hard timeouts to the harness; the `run` line gives attribution.
6. **ROADMAP/DESIGN §8.2:** document the `run` line and the timing suffix.

## Acceptance criteria

- `src/ktest.rs` gone; no ktest file over 800 lines.
- `make test-kernel` output shows `run`/`ok` pairs and a slowest-tests summary; a deliberately hung test (temporary) yields `ktest hung in <name>`.
- 103 tests still pass on `-smp 2` and `-smp 4` and the LAPIC fallback.

## Tests

Harness unit tests; the temporary hang test during development only.

## Risks and rollback

The protocol change affects `check_ktest_output`; land the harness change first (tolerant of missing `run` lines), then the kernel change.
