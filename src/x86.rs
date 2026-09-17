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

/// Read CR3 (page-table root physical address). Bits 0..12 are flags
/// (PCID etc); the physical address lives in bits 12..52.
#[inline]
pub fn read_cr3() -> u64 {
    let val: u64;
    unsafe { asm!("mov {}, cr3", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// Write CR3. Reloads the entire non-global TLB.
///
/// # Safety
/// `cr3` must point at a valid PML4 whose top-level entries cover every
/// VA the CPU may touch between now and the next `mov cr3`, including
/// the current RIP and RSP.
#[inline]
pub unsafe fn write_cr3(cr3: u64) {
    unsafe { asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags)) };
}

/// `invlpg` for a single virtual address. Cheap enough that every leaf
/// edit calls it; DESIGN §4.3 requires it after any single-PTE change.
///
/// `#[allow(dead_code)]` because slice B only exercises this from the
/// (phase-2-wired) MMIO patch path; phase 2 turns it into a used symbol
/// without editing this file.
#[inline]
#[allow(dead_code)]
pub fn invlpg(va: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags)) };
}

/// Read an MSR by index.
#[inline]
pub fn rdmsr(msr: u32) -> u64 {
    let hi: u32;
    let lo: u32;
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

/// Write an MSR by index.
///
/// # Safety
/// Touching the wrong MSR can wedge the CPU. Caller vouches for `msr`.
#[inline]
pub unsafe fn wrmsr(msr: u32, val: u64) {
    let lo = (val & 0xFFFF_FFFF) as u32;
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

pub const IA32_EFER: u32 = 0xC000_0080;
pub const EFER_NXE: u64 = 1 << 11;

/// Read `rsp`. Used by paging bring-up to find which top-level PML4
/// entry covers Limine's boot stack, so the switch to our own PML4
/// survives the following `mov cr3`.
///
/// `#[allow(dead_code)]` for the panic-test build, which never reaches
/// paging init.
#[inline]
#[allow(dead_code)]
pub fn read_rsp() -> u64 {
    let val: u64;
    unsafe { asm!("mov {}, rsp", out(reg) val, options(nomem, nostack, preserves_flags)) };
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

/// Save `RFLAGS.IF`, `cli`, restore on drop. DESIGN §2.3: the heap
/// (and later the buddy) is taken with interrupts off. Nested guards
/// are fine: only the outermost restores IF.
pub struct InterruptGuard {
    restore: bool,
}

impl InterruptGuard {
    pub fn enter() -> Self {
        let rflags: u64;
        unsafe {
            asm!(
                "pushfq",
                "pop {0}",
                "cli",
                out(reg) rflags,
            );
        }
        Self {
            restore: rflags & (1 << 9) != 0,
        }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        if self.restore {
            unsafe { asm!("sti", options(nomem, nostack)) };
        }
    }
}

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a 32-bit write.
#[inline]
#[allow(dead_code)]
pub unsafe fn outl(port: u16, val: u32) {
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") val,
            options(nomem, nostack, preserves_flags)
        )
    };
}

/// Read `cr2` (page-fault address). Used by the ktest scoped #PF catcher.
#[inline]
#[allow(dead_code)]
pub fn read_cr2() -> u64 {
    let val: u64;
    unsafe { asm!("mov {}, cr2", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// Current code selector. The ktest IDT needs it for gate descriptors.
#[inline]
#[allow(dead_code)]
pub fn read_cs() -> u16 {
    let val: u16;
    unsafe { asm!("mov {0:x}, cs", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// Task register. In-guest GDT test checks we `ltr`'d the TSS selector.
#[inline]
#[allow(dead_code)]
pub fn read_tr() -> u16 {
    let val: u16;
    unsafe { asm!("str {0:x}", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// 10-byte GDTR/IDTR payload.
#[repr(C, packed)]
pub struct DtPtr {
    pub limit: u16,
    pub base: u64,
}

/// # Safety
/// `ptr` must describe a valid GDT that covers every selector we load
/// immediately after, including the code selector used by `retfq`.
#[inline]
pub unsafe fn lgdt(ptr: &DtPtr) {
    unsafe {
        asm!(
            "lgdt [{}]",
            in(reg) ptr,
            options(readonly, nostack, preserves_flags)
        )
    };
}

/// # Safety
/// `ptr` must describe a 256-entry IDT. Hardware IRQs should already
/// be masked at the controller.
#[inline]
pub unsafe fn lidt(ptr: &DtPtr) {
    unsafe {
        asm!(
            "lidt [{}]",
            in(reg) ptr,
            options(readonly, nostack, preserves_flags)
        )
    };
}

/// # Safety
/// `sel` must index an available 64-bit TSS descriptor in the current GDT.
#[inline]
pub unsafe fn ltr(sel: u16) {
    unsafe { asm!("ltr {0:x}", in(reg) sel, options(nomem, nostack, preserves_flags)) };
}
