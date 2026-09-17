//! Tiny x86_64 primitives. Everything here is intentionally small; real
//! CPU state work lives under `src/arch/` in later phases.

// The MSR / CR3 / invlpg helpers only run from the paging bringup, which
// panic-test builds skip. Silence dead-code chatter in that build.
#![cfg_attr(feature = "panic-test", allow(dead_code))]

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

/// Read a Model-Specific Register.
///
/// # Safety
/// `msr` must name a real MSR; reading an unimplemented MSR raises `#GP`.
#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Write a Model-Specific Register.
///
/// # Safety
/// `msr` and `val` must be valid; writes to reserved bits raise `#GP` and
/// misuse of EFER / GSBASE / etc. corrupts CPU state.
#[inline]
pub unsafe fn wrmsr(msr: u32, val: u64) {
    let lo = val as u32;
    let hi = (val >> 32) as u32;
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") lo,
            in("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Read CR3 (physical base of the active PML4, plus flags).
#[inline]
pub fn read_cr3() -> u64 {
    let val: u64;
    unsafe {
        asm!("mov {}, cr3", out(reg) val, options(nomem, nostack, preserves_flags));
    }
    val
}

/// Write CR3. Reloads the top-level page-table pointer and flushes the
/// non-global TLB entries on this CPU. `GLOBAL` mappings survive the
/// reload (DESIGN §4.3), which is why unmapping a kernel page requires an
/// `invlpg` instead of hoping CR3 washed it out.
///
/// # Safety
/// `val` must be a valid PML4 physical address with appropriate low
/// flag bits (usually zero). The mapping must cover RIP, RSP, and the
/// current GDT/IDT before this executes.
#[inline]
pub unsafe fn write_cr3(val: u64) {
    unsafe {
        asm!("mov cr3, {}", in(reg) val, options(nostack, preserves_flags));
    }
}

/// Invalidate the TLB entry for one virtual address on this CPU.
/// Required after any single-PTE edit (DESIGN §4.3).
///
/// # Safety
/// Safe with respect to memory; `invlpg` on a non-canonical address is
/// a no-op on x86_64 by architectural definition, but callers should
/// pass real addresses to keep the callsite meaningful.
#[inline]
#[allow(dead_code)] // phase 2 wires up the first caller (MMIO patch after ACPI)
pub unsafe fn invlpg(virt: u64) {
    unsafe {
        asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags));
    }
}

// IA32_EFER (MSR 0xC0000080). Bit 8 = LME, bit 11 = NXE.
pub const IA32_EFER: u32 = 0xC000_0080;
pub const EFER_NXE: u64 = 1 << 11;

/// Set the NX-enable bit in EFER if it is not already on. NX must be live
/// before the kernel PML4 (which sets NX bits on every non-code mapping)
/// is installed, otherwise every kernel page turns into a reserved-bit
/// fault the moment CR3 loads (DESIGN §9.5 for the AP variant of this bug).
///
/// # Safety
/// Runs at CPL 0. Modifies a global CPU-wide setting.
#[inline]
pub unsafe fn enable_nxe() {
    let cur = unsafe { rdmsr(IA32_EFER) };
    if cur & EFER_NXE == 0 {
        unsafe { wrmsr(IA32_EFER, cur | EFER_NXE) };
    }
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
