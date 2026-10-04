//! aarch64 boot helpers: exception level, ISA floor, early vectors.

use vibeos::arch::BootHandover;

use super::{Arch, cpu, vectors};
use crate::boot::{self, BootInfo};

/// After serial: EL marker, ISA floor, early VBAR.
pub fn early_init() {
    cpu::note_exception_level();
    cpu::check_isa_floor();
    vectors::init_early();
}

impl BootHandover for Arch {
    type Info = BootInfo;

    #[inline]
    fn info() -> &'static BootInfo {
        boot::info()
    }
}
