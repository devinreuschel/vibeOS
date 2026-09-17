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

/// Phase 2 §2.4. Runtime table count via `writeln!`; harness pins the
/// shape with `and_contains`.
pub const ACPI_XSDT_PREFIX: &str = "vibeOS: acpi: xsdt ";
pub const ACPI_XSDT_SUFFIX: &str = " tables";

/// Last marker of the current boot contract. Phase 1 closed the memory
/// exit gate; later phases replace this with `shell ready` (DESIGN §3.3).
pub const BOOT_DONE: &str = "vibeOS: boot: phase1 done";

/// Panic banner. Kept short so the panic path allocates nothing.
pub const PANIC_BANNER: &str = "vibeOS: panic:";
