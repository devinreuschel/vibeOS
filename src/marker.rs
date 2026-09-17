//! Serial marker strings, DESIGN §2.6.
//!
//! Every boot line is `vibeOS: <subsystem>: <state>` (lowercase, no trailing
//! punctuation). The e2e harness asserts these in order. Adding a marker
//! means updating the harness contract in the same commit.

pub const SERIAL_ONLINE: &str = "vibeOS: serial online";
pub const LIMINE_OK: &str = "vibeOS: limine: rev 3 ok";
pub const BOOT_DONE: &str = "vibeOS: boot: phase0 done";

/// Panic banner. Kept short so the panic path allocates nothing.
pub const PANIC_BANNER: &str = "vibeOS: panic:";
