# Q3 · One boot-cell primitive; retire `static mut` and `&'static mut` accessors

| | |
|---|---|
| **Area** | 4.2 Code quality & consistency |
| **Impact / Effort / Phase** | Medium / S (primitive) + M (migration) / II |
| **Depends on** | — |
| **Blocks** | A3 step 5, D1, D3 |
| **Review** | [ARCHITECTURE_REVIEW.md §4.2](../ARCHITECTURE_REVIEW.md#42-code-quality--consistency) |

## Problem

- 17 files define their own `struct Cell<T>(UnsafeCell<T>); unsafe impl<T> Sync for Cell<T> {}` or a `BootCell` variant: `acpi_init.rs:17`, `apic_init.rs:37`, `fat_init.rs:27`, `irq_init.rs:32`, `kbd_init.rs:31`, `kva_init.rs:18`, `log_init.rs:22`, `per_cpu_init.rs:21`, `pci_init.rs:49`, `smp_init.rs:34`, `shell_init.rs:30`, `arch/gdt.rs:23`, `vibefs_init.rs:29`, `time_init.rs:46`, `work_init.rs:18`, `arch/catch.rs:17`, `arch/idt.rs:15`.
- `static mut`: `acpi_init.rs:35 MMIO_UC`, `file_init.rs:131 CWD_BUF`, `paging_init.rs:112 IOREMAP`, `arch/catch.rs:77 vibeos_jmpbuf` (extern, asm-owned).
- Functions returning `&'static mut`: `irq_init::{pool, routes, th}`, `pci_init::ecam`, `work_init::st`, `per_cpu_init::{cpu_mut, current_mut, try_current_mut}` (13 call sites), `thread_init::current_tcb`. Two live `&mut` to one object is UB regardless of IRQ state; DESIGN §9.4 already records one such bug.

## Recommended fix

One `BootCell<T>` (write once before SMP and IRQs, then shared) and one `IrqCell<T>` (closure-scoped exclusive access with IRQs off and a same-CPU re-entry check) in `src/cell.rs`. Replace every copy. Convert `&'static mut` accessors to closures or to shared references over interior-mutable fields.

## Implementation plan

1. **Primitive** `src/cell.rs`:
   - `BootCell<T>`: `const fn new()`, `unsafe fn set(&self, T)` (debug-asserts unset; must run before `smp: done`), `fn get(&self) -> &T` (panics if unset), `fn try_get(&self) -> Option<&T>`; state in an `AtomicU8`.
   - `IrqCell<T>`: `const fn new(T)`, `fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R` taking an `InterruptGuard` and an `AtomicBool` busy flag that panics on re-entry. For data shared across CPUs use `SpinMutex`, not `IrqCell`.
   - DESIGN §2.3: "three cells: `SpinMutex` (shared), `IrqCell` (CPU-local or boot-only mutable), `BootCell` (write-once)".
2. **Migrate the 17 wrappers**, one commit each. Most become `BootCell` (tables, ACPI info, GDT/IDT, trampoline params); `log_init::LOG` and `shell_init::REG` become `IrqCell` (they already hand-roll the TAS-plus-IRQ pattern).
3. **`static mut` removals:** `MMIO_UC` → `AtomicBool`; `CWD_BUF` → `IrqCell<[u8; MAX_PATH]>` or fold into the `FILES` `SpinMutex`; `IOREMAP` → `SpinMutex<IoremapWindow>` at `RANK_PT` (verify it is only touched under `paging_init::with_pt`). `vibeos_jmpbuf` stays, documented.
4. **Accessor rewrite:** `irq_init::{pool, routes, th}`, `work_init::st`, `pci_init::ecam` → `IrqCell::with`. `per_cpu_init::current_mut` (13 sites): where the field is a counter or flag, make it atomic and use `current()`; where it is the run queue or `current` pointer (owner-CPU-only, IRQ-off), expose `with_current(|c: &mut PerCpu|)` with a debug re-entry flag. `thread_init::current_tcb` → `*mut Tcb` or `&Tcb` with atomic state fields.
5. **Lint:** ensure `static_mut_refs` (deny-by-default in edition 2024) is not allowed anywhere; add a grep guard for `unsafe impl<T> Sync` outside `cell.rs` to `make check`.

## Acceptance criteria

- `grep -rn 'unsafe impl<T> Sync' src` matches only `src/cell.rs`.
- `grep -rn 'static mut' src` matches only `arch/catch.rs`.
- No function signature returns `&'static mut`.
- All tiers green, including `alloc_stress_smp`, `spawn_exit_thousands`, `cross_cpu_spawn`.

## Tests

In-guest: `irqcell_reentry_panics` (behind `arch::catch`), `bootcell_set_once`.

## Risks and rollback

`per_cpu` accessor changes touch the scheduler hot path; keep the closure `#[inline(always)]` and compare ktest timings before and after. One module per commit; revert individually.
