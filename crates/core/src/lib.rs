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
// Test code is exempt at the root (ROADMAP §10.1, C-LINTS): a host test may
// unwrap, discard a result, and use `alloc`'s owning types, since a failure
// ends the test, not the kernel.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::let_underscore_must_use,
        clippy::unused_result_ok,
        clippy::disallowed_types,
        clippy::disallowed_macros
    )
)]

pub mod acpi;
pub mod arch;
pub mod atomic;
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub mod block;
// Kernel `mod cell` in main.rs. Host tests only: production vibeos-core
// has no InterruptGuard / per_cpu_init.
#[cfg(test)]
#[path = "../../../src/cell.rs"]
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub mod cell;
pub mod console;
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub mod dev;
pub mod drivers;
pub mod fmt_util;
pub mod fs;
pub mod irq;
#[allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "kalloc wraps alloc's owning types (DESIGN §4.4)"
)]
pub mod kalloc;
pub mod limits;
pub mod log;
pub mod marker;
#[allow(clippy::missing_safety_doc, reason = "audit pending, ROADMAP §10.1")]
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub mod mm;
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub mod proc;
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
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
