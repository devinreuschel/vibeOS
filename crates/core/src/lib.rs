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
pub mod block;
pub mod boot;
// The cells, over a port's seam (DESIGN §2.3); the kernel names them over
// its port in `src/cell.rs`.
pub mod cell;
pub mod console;
pub mod dev;
pub mod drivers;
pub mod fmt_util;
pub mod fs;
pub mod irq;
#[allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "permanent: kalloc wraps alloc's owning types, and everything it exposes is fallible (DESIGN §4.4)"
)]
pub mod kalloc;
pub mod kerror;
pub mod ktest;
pub mod limits;
pub mod log;
pub mod machine;
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

pub use arch::{apic, desc, pic, uart, vectors};
pub use block::{cache, part};
pub use console::{fb, font, kbd};
pub use dev::{dma, entropy, pci, virtio};
pub use drivers::{virtio_blk, virtio_input};
pub use fs::{fat, vibefs};
pub use irq::ipi;
pub use mm::{asid, heap, kva, paging, physmap, pmm};
pub use proc::{addr_space, elf, syscall};
pub use sched::{fpu, thread, wait, work};
pub use smp::per_cpu;
pub use sync::lock;
