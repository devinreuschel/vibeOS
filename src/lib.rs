//! vibeOS: portable, host-testable half.
//!
//! Anything that has no hardware access lives here so `cargo test --lib`
//! covers it. Serial byte formatting, marker strings, small utilities.
//! Hardware pokes live in the binary crate. See DESIGN §1.1.

#![cfg_attr(not(test), no_std)]

pub mod acpi;
pub mod addr_space;
pub mod apic;
pub mod block;
pub mod cache;
pub mod console;
pub mod desc;
pub mod dev;
pub mod dma;
pub mod fb;
pub mod fat;
pub mod font;
pub mod vibefs;
pub mod fs;
pub mod fmt_util;
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
pub mod sched;
pub mod shell;
pub mod smp;
pub mod syscall;
pub mod sync;
pub mod symtab;
pub mod thread;
pub mod time;
pub mod uart;
pub mod vectors;
pub mod virtio;
pub mod virtio_blk;
pub mod wait;
pub mod work;
