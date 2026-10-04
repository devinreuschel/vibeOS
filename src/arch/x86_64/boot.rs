//! `BootHandover` on x86_64 (PORTABILITY §11.1): the Limine responses
//! `boot::capture` normalizes into `BootInfo`.

use vibeos::arch::BootHandover;

use super::Arch;
use crate::boot::{self, BootInfo};

impl BootHandover for Arch {
    type Info = BootInfo;

    #[inline]
    fn info() -> &'static BootInfo {
        boot::info()
    }
}
