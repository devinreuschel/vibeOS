# A4 · Break the mutual dependencies between kernel modules

**ROADMAP:** the §10.3 and §10.7 boxes that cite A4. Where this plan and those boxes differ, the boxes decide. Superseded here: the Problem's table, which is the review's count of nine; the §10.3 box breaks every two-way dependency `scripts/check_cycles.py` reports.

| | |
|---|---|
| **Area** | 4.1 Architecture & module boundaries |
| **Impact / Effort / Phase** | Medium / M / III |
| **Depends on** | A3 (fs cluster), A1 step 4 (shell commands move) |
| **Blocks** | — |
| **Review** | [ARCHITECTURE_REVIEW.md §4.1](../ARCHITECTURE_REVIEW.md#41-architecture--module-boundaries) |

## Problem

A `use crate::` graph over the kernel half (212 edges) has nine two-cycles:

| Cycle | Evidence |
|---|---|
| `serial ↔ log_init` | `src/serial.rs:64–65` calls `log_init::is_emitting` / `capture_serial`; `src/log_init.rs:18,157` uses `Serial` |
| `serial → ipi_init` | `src/serial.rs:64,73` `ipi_init::is_halting()`; `ipi_init` prints through `serial` |
| `sync_init ↔ thread_init` | `src/sync_init.rs:22` uses `thread_init` for blocking primitives; `src/thread_init.rs:30` uses `SpinMutex` |
| `sync_init → ipi_init` | `src/sync_init.rs:60` `ipi_init::service_incoming()` inside `SpinMutex::lock` (documented deadlock avoidance) |
| `ipi_init ↔ thread_init` | `src/thread_init.rs:186,339` `ipi_init::drain_inbox()`; `src/ipi_init.rs:18` |
| `dev_init ↔ pci_init` | `src/pci_init.rs:16` registers into `dev_init`; `src/dev_init.rs:11` |
| `fs_init ↔ fat_init / vibefs_init / file_init` | `src/fs_init.rs:14–18`; backends import `fs_init` |
| `file_init ↔ shell_init` | `src/shell_init.rs:157` `file_init::complete_line`; `src/file_init.rs:21` registers commands |
| `vibefs_init → file_init` | `src/vibefs_init.rs:580` (crash loop uses the File API) |

DESIGN §1.1 rule 6 says lower layers do not call up; `serial` is the lowest cross-cutting layer and calls two layers up.

## Recommended fix

Split "raw" from "policy" at the bottom (serial), replace upward calls with hooks installed at init, and let the fs cluster collapse as A3 lands. Add a cycle check to `make check`.

## Implementation plan

1. **`serial` raw layer.** Split `src/serial.rs` into `serial/raw.rs` (port I/O, the `TX` lock, `write_bytes_raw`, `try_write_bytes`, `HALTING: AtomicBool` + `set_halting()`, the dump owner's CPU id, which replaces `panic.rs`'s `DUMPING`, and `write_owner()`, which takes no lock and no `InterruptGuard` and writes only on the owner; once `HALTING` is set, a write on any other CPU calls a stop hook that `ipi_init::init` installs, as step 2 installs `SPIN_POLL` (DESIGN §2.5 step 1)) and `serial/mod.rs` (`Serial`, `PlainSerial`, `line`, which call `log_init::capture_serial`). `ipi_init::halt_others` calls `serial::raw::set_halting()` (downward). `log_init` and `panic` import only `serial::raw`.
2. **Spin hook.** In `sync_init`: a `static SPIN_POLL: AtomicPtr<()>` holding an `fn()`; `SpinMutex::lock` calls it in the spin loop if set. `ipi_init::init` installs `service_incoming`. `sync_init` no longer imports `ipi_init`. Note the hook in DESIGN §7.9.
3. **Split sync.** `sync/spin.rs` (`SpinMutex`, `InterruptGuard`, rank accounting; no thread dependency) and `sync/blocking.rs` (`BlockingMutex`, `Condvar`, `RwLock`, `Semaphore`, `Channel`; depends on thread). `thread_init` imports `sync::spin` only.
4. **IPI ↔ thread.** The inbox's push and drain live in `vibeos-core` (ROADMAP §10.8); the `0xFD` handler's call to the drain moves into `thread_init` (the inbox is scheduler state; `ipi_init` only raises the vector). `ipi_init` reaches the scheduler through a small `SchedHooks { wake_inbox: fn(u32), reschedule: fn() }` installed by `sched_init::init`. `thread_init → ipi_init::send_reschedule(cpu)` remains (downward).
5. **PCI → dev.** `pci_init::scan() -> impl Iterator<Item = Device>`; `dev_init::init` consumes it and fills the registry. Remove `use crate::dev_init` from `pci_init`.
6. **fs cluster.** After A3, backends implement `InodeOps` and need only lib types; remove `use crate::fs_init` from `fat_init` / `vibefs_init`. Move `file_init::init()` command registration and `complete_line` to `shell/cmds` and `shell/complete` (A1 step 4). Move `vibefs_init::crash_loop` to `src/vibefs_crash.rs` (feature-gated top-layer module).
7. **Guard.** `scripts/check_cycles.py`: parse `use crate::x` and `crate::x::` per file, build the graph, fail on any two-cycle, print longer cycles as warnings. Run from `make check` (DX1).

## Acceptance criteria

- `scripts/check_cycles.py` reports zero two-cycles.
- `src/serial/raw.rs` imports nothing from `crate::` except `x86` and `sync::spin`; `write_owner` takes no lock and no `InterruptGuard`.
- All tiers green; panic e2e still shows `rust_begin_unwind` and `logrec` lines (the serial split must not lose capture).

## Tests

Host: unit test for the cycle checker on a fixture graph. In-guest: existing `spin_mutex`, `tlb_shootdown_remote`, `reschedule_ipi_wake_ap` cover the hook paths; add `spin_poll_hook_installed`.

## Risks and rollback

Hooks add one indirect call on the spin path of `SpinMutex::lock` only (not the uncontended fast path). Steps 1–2 are independent and low risk; do them first.

## Out of scope

Cycles longer than two are reported, not fixed, here.
