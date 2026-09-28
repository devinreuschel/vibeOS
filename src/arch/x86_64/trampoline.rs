//! AP trampoline blob. DESIGN §7.3 / ROADMAP §4.4.
//!
//! `global_asm!` so the kernel does not shell out to nasm. `smp_init`
//! copies `__trampoline_start..__trampoline_end` to physical 0x8000.

use core::arch::global_asm;

global_asm!(include_str!("trampoline.S"));
