//! Tiny x86_64 primitives. Everything here is intentionally small; real
//! CPU state work lives under `src/arch/` in later phases.

use core::arch::asm;

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a byte write.
#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags)) };
}

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a byte read.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe { asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags)) };
    val
}

/// `cli; hlt` loop. Never returns.
///
/// Used by the panic handler and by the boot path once phase 0 has printed
/// its final marker. Interrupts are disabled so no ISR can drag us back.
#[inline]
pub fn halt() -> ! {
    loop {
        unsafe { asm!("cli; hlt", options(nomem, nostack)) };
    }
}
