//! The kernel's names for the cells (DESIGN §2.3), which live in
//! `vibeos::cell` over the port's seam, and the kernel's `CellHooks`.
//!
//! `crate::cell::IrqCell<T>` is `vibeos::cell::IrqCell<T, arch::current::Arch>`
//! (named once, in `arch::current`), and `crate::cell::BootCell` is
//! `vibeos::cell::BootCell`.

pub use crate::arch::current::IrqCell;
pub use vibeos::cell::BootCell;
pub(crate) use vibeos::{assert_impl, assert_not_impl};

impl vibeos::cell::CellHooks for crate::arch::current::Arch {
    /// Refuse an `IrqCell` inside a lockless section (DESIGN §2.2's last
    /// row), as `SpinMutex::lock` is.
    #[inline(always)]
    #[track_caller]
    fn acquire_check() {
        crate::sync_init::check_cell_context();
    }
}
