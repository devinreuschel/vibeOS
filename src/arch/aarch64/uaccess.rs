//! User-memory accessors. EL0 is Phase 11 S9; copies report uncopied.

use vibeos::arch::UserAccess;

use super::Arch;

impl UserAccess for Arch {
    unsafe fn copy_in(_dst: *mut u8, _src: u64, len: usize) -> usize {
        len
    }

    unsafe fn copy_out(_dst: u64, _src: *const u8, len: usize) -> usize {
        len
    }
}
