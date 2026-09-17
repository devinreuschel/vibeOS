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

/// Phase 1 slice B markers. `paging: cr3 ok` fires once the kernel is
/// running on its own PML4 (DESIGN §3.3 step 7 / §8.3). `paging: mmio uc`
/// fires right after, per DESIGN §3.3 step 8, even when there are no
/// device MMIO regions to patch yet (ACPI arrives in phase 2). The
/// marker records that the step ran in the correct place; the physmap
/// patch is a no-op call today and becomes real when APIC/HPET land.
pub const PAGING_CR3_OK: &str = "vibeOS: paging: cr3 ok";
pub const PAGING_MMIO_UC: &str = "vibeOS: paging: mmio uc";

pub const BOOT_DONE: &str = "vibeOS: boot: phase0 done";

/// Panic banner. Kept short so the panic path allocates nothing.
pub const PANIC_BANNER: &str = "vibeOS: panic:";
