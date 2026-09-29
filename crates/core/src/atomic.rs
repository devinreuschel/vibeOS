//! The atomics seam (C-ATOMICS, ROADMAP §10.8).
//!
//! `vibeos-core` names its atomic types, `fence`, `compiler_fence` and
//! `spin_loop` through this module, so a loom model (`#[cfg(all(test, loom))]`,
//! built with `RUSTFLAGS="--cfg loom"`) runs the same code over loom's
//! atomics. Outside `cfg(loom)` these are `core`'s.
//!
//! Loom's atomics have no `const fn new`, so a `static` takes `core`'s from
//! [`statics`] in both configurations; so does an atomic field of a
//! `#[repr(C)]` type whose layout asm or a const offset assertion fixes
//! (`PerCpu`). Both stay out of every loom model. `scripts/check_atomics.py`
//! fails on `core::sync::atomic` anywhere else in the crate outside test code.

#[cfg(not(loom))]
pub use core::hint::spin_loop;
#[cfg(not(loom))]
pub use core::sync::atomic::{
    AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicPtr, AtomicU8,
    AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering, compiler_fence, fence,
};

#[cfg(loom)]
pub use loom::hint::spin_loop;
#[cfg(loom)]
pub use loom::sync::atomic::{
    AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicPtr, AtomicU8,
    AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

/// Shim: loom has no `compiler_fence`. A full fence orders at least as
/// much, so it stands in for one in a model.
#[cfg(loom)]
pub fn compiler_fence(order: Ordering) {
    fence(order);
}

/// `std::thread_local!` for the stub port's host build, loom's under
/// `cfg(loom)`.
#[cfg(all(not(loom), any(test, feature = "std")))]
pub use std::thread_local;

#[cfg(loom)]
pub use loom::thread_local;

/// `core`'s atomics in both configurations, for `static`s and for the
/// layout-fixed `#[repr(C)]` fields (C-ATOMICS).
pub mod statics {
    pub use core::sync::atomic::{
        AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicPtr, AtomicU8,
        AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
    };
}
