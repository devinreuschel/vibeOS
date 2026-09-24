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
pub mod addr_space;
pub mod apic;
pub mod block;
pub mod cache;
// Kernel `mod cell` in main.rs. Host tests only: production vibeos-core
// has no InterruptGuard / per_cpu_init.
#[cfg(test)]
pub mod cell;
pub mod console;
pub mod desc;
pub mod dev;
pub mod dma;
pub mod elf;
pub mod entropy;
pub mod fat;
pub mod fb;
pub mod fmt_util;
pub mod font;
pub mod fs;
pub mod heap;
pub mod ipi;
pub mod irq;
pub mod kbd;
pub mod kva;
pub mod lock;
pub mod log;
pub mod marker;
pub mod paging;
pub mod part;
pub mod pci;
pub mod per_cpu;
pub mod pic;
pub mod pmm;
pub mod proc;
pub mod sched;
pub mod shell;
pub mod smp;
pub mod symtab;
pub mod sync;
pub mod syscall;
pub mod thread;
pub mod time;
pub mod uart;
pub mod vectors;
pub mod vibefs;
pub mod virtio;
pub mod virtio_blk;
pub mod wait;
pub mod work;
