//! Serial marker strings, DESIGN §2.6.
//!
//! Every boot line is `vibeOS: <subsystem>: <state>` (lowercase, no trailing
//! punctuation). The e2e harness asserts these in order. Adding a marker
//! means updating the harness contract in the same commit.

pub const SERIAL_ONLINE: &str = "vibeOS: serial online";
pub const LIMINE_OK: &str = "vibeOS: limine: rev 3 ok";

/// Substring the harness matches for the phase-1 PMM marker. The full
/// line is emitted with a runtime frame count via `writeln!`, so tests
/// assert on the shape rather than the full string. See DESIGN §8.3.
pub const PMM_PREFIX: &str = "vibeOS: pmm: ";
pub const PMM_FREE_SUFFIX: &str = " free 4KiB frames";

/// Phase 1 §1.2 exit-gate marker. Emitted the instant CR3 is loaded with
/// our own PML4 so the harness can pin the moment we own the address
/// space, not merely built its tables.
pub const PAGING_CR3_OK: &str = "vibeOS: paging: cr3 ok";

/// DESIGN §3.3 step 8. Emitted only after `patch_physmap_uc` actually
/// touches a discovered LAPIC / I/O APIC / HPET leaf — never as a hollow
/// claim.
pub const PAGING_MMIO_UC: &str = "vibeOS: paging: mmio uc";

/// Phase 1 §1.4 / §1.5 exit-gate markers. DESIGN §3.3 steps 9 and 10.
pub const HEAP_OK: &str = "vibeOS: heap ok";
pub const KVA_READY: &str = "vibeOS: kva: ready";

/// Phase 2 slice A. Live order is after KVA (IST stacks come from it);
/// relative order matches DESIGN §3.3 steps 3–5.
pub const GDT_OK: &str = "vibeOS: gdt ok";
/// PIC boot step finished: ICW remap+mask ran, or FADT skip. Not a claim
/// that ports were programmed (unlike `paging: mmio uc`).
pub const PIC_REMAPPED: &str = "vibeOS: pic: remapped";
pub const IDT_OK: &str = "vibeOS: idt ok";

/// DESIGN §3.3 step 11. After IDT (GDT already loaded). Before ACPI marker.
pub const PER_CPU_BSP: &str = "vibeOS: per_cpu: bsp ready";

/// Phase 2 §2.4. Runtime table count via `writeln!`; harness pins the
/// shape with `and_contains`. Live order is after IDT (step 12 after 3–5).
pub const ACPI_XSDT_PREFIX: &str = "vibeOS: acpi: xsdt ";
pub const ACPI_XSDT_SUFFIX: &str = " tables";

/// Phase 2 §2.6. Runtime frequency via `writeln!`. DESIGN §3.3 step 13.
pub const TIME_TSC_PREFIX: &str = "vibeOS: time: tsc ";
pub const TIME_TSC_SUFFIX: &str = "/ms";

/// Phase 4 §4.3. Runtime mode via `writeln!`. ROADMAP / #26 spelling.
pub const TIME_LAPIC_PREFIX: &str = "vibeOS: time: lapic_timer ok (";
pub const TIME_LAPIC_SUFFIX: &str = ")";

/// Phase 3 slice B. DESIGN §3.3 steps 14 and 16. `irq: enabled` is after
/// `sched: cpu0 ready`. Slice C emits `sched: cpu<i> ready` on each AP
/// before `smp: ap online`.
pub const SCHED_CPU0: &str = "vibeOS: sched: cpu0 ready";
pub const SCHED_CPU_PREFIX: &str = "vibeOS: sched: cpu";
pub const SCHED_CPU_SUFFIX: &str = " ready";
pub const IRQ_ENABLED: &str = "vibeOS: irq: enabled";

/// Phase 4 slice B. After `irq: enabled`, before `boot: phase1 done`.
/// Exactly `N-1` `ap online` lines at `-smp N`, then `smp: done`.
pub const SMP_AP_ONLINE: &str = "vibeOS: smp: ap online";
pub const SMP_DONE: &str = "vibeOS: smp: done";

/// Last marker of the current boot contract. Phase 1 closed the memory
/// exit gate; later phases replace this with `shell ready` (DESIGN §3.3).
pub const BOOT_DONE: &str = "vibeOS: boot: phase1 done";

/// Phase 5 slice B. After `smp: done` (and the phase-1 stand-in). FB text,
/// PS/2, and the mux are live; IRQ1 was unmasked after the handler.
pub const CONSOLE_OK: &str = "vibeOS: console ok";

/// Panic banner. Kept short so the panic path allocates nothing.
pub const PANIC_BANNER: &str = "vibeOS: panic:";

/// End of a panic/exception dump. Harness waits for this after a signature.
pub const PANIC_HALTED: &str = "vibeOS: panic: halted";
