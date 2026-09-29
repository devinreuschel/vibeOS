//! vibeOS: portable, host-testable half (`vibeos-core`).
//!
//! Anything that has no hardware access lives here so host `cargo test`
//! covers it. Serial byte formatting, marker strings, small utilities.
//! Hardware pokes live in the binary crate. See DESIGN §1.1.
//!
//! Restriction lints (E1 / DESIGN §2.5): the portable half returns the
//! module error instead of panicking on data. `indexing_slicing` and
//! `arithmetic_side_effects` are denied per byte parser, not here (ROADMAP
//! §10.1): most of the crate's indexing is bounded by construction.

#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod acpi;
pub mod arch;
pub mod block;
// Kernel `mod cell` in main.rs. Host tests only: production vibeos-core
// has no InterruptGuard / per_cpu_init.
#[cfg(test)]
#[path = "../../../src/cell.rs"]
pub mod cell;
pub mod console;
pub mod dev;
pub mod drivers;
pub mod fmt_util;
pub mod fs;
pub mod irq;
pub mod kalloc;
pub mod limits;
pub mod log;
pub mod marker;
pub mod mm;
pub mod proc;
pub mod sched;
pub mod shell;
pub mod smp;
pub mod symtab;
pub mod sync;
pub mod time;
pub mod trap;

pub use arch::x86_64::{apic, desc, pic, uart, vectors};
pub use block::{cache, part};
pub use console::{fb, font, kbd};
pub use dev::{dma, entropy, pci, virtio};
pub use drivers::virtio_blk;
pub use fs::{fat, vibefs};
pub use irq::ipi;
pub use mm::{heap, kva, paging, pmm};
pub use proc::{addr_space, elf, syscall};
pub use sched::{fpu, thread, wait, work};
pub use smp::per_cpu;
pub use sync::lock;
