//! User-memory access: the kernel half of `vibeos::proc::uaccess` over this
//! build's port (C-UACCESS, INTERRUPTS §5.1). Each copy runs the range
//! check, then the port's copy inside its user-access window, whose fault
//! resumes at an exception-table fixup.

use vibeos::proc::uaccess::{self, Fault};

use crate::arch::current::Arch;

/// Copy `dst.len()` bytes from user address `src`, all or nothing.
pub(crate) fn copy_from_user(dst: &mut [u8], src: u64) -> Result<(), Fault> {
    uaccess::copy_from_user::<Arch>(dst, src)
}

/// Copy `src` to user address `dst`, all or nothing.
pub(crate) fn copy_to_user(dst: u64, src: &[u8]) -> Result<(), Fault> {
    uaccess::copy_to_user::<Arch>(dst, src)
}

/// Copy from user address `src` into `dst`; the bytes copied before a
/// fault, 0 for a refused range.
pub(crate) fn copy_from_user_partial(dst: &mut [u8], src: u64) -> usize {
    uaccess::copy_from_user_partial::<Arch>(dst, src)
}

/// Copy `src` to user address `dst`; the bytes copied before a fault, 0
/// for a refused range.
pub(crate) fn copy_to_user_partial(dst: u64, src: &[u8]) -> usize {
    uaccess::copy_to_user_partial::<Arch>(dst, src)
}

/// Copy the NUL-terminated user string at `src` into `out`; its length,
/// or `out.len()` when `out` filled before a NUL.
pub(crate) fn strncpy_from_user(out: &mut [u8], src: u64) -> Result<usize, Fault> {
    uaccess::strncpy_from_user::<Arch>(out, src)
}
