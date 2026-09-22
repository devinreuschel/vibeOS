//! Tiny x86_64 primitives. Everything here is intentionally small; real
//! CPU state work lives under `src/arch/` in later phases.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a byte write.
#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags))
    };
}

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a byte read.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe {
        asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags))
    };
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
pub const EFER_SCE: u64 = 1 << 0;
pub const IA32_STAR: u32 = 0xC000_0081;
pub const IA32_LSTAR: u32 = 0xC000_0082;
pub const IA32_FMASK: u32 = 0xC000_0084;
pub const IA32_FS_BASE: u32 = 0xC000_0100;
pub const IA32_GS_BASE: u32 = 0xC000_0101;
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// TF|IF|DF|IOPL|NT|AC. Cleared on `syscall`.
pub const FMASK_SYSCALL: u64 = 0x47700;

pub const CR0_EM: u64 = 1 << 2;
pub const CR0_MP: u64 = 1 << 1;
pub const CR0_TS: u64 = 1 << 3;
pub const CR0_WP: u64 = 1 << 16;
pub const CR4_OSFXSR: u64 = 1 << 9;
pub const CR4_UMIP: u64 = 1 << 11;
pub const CR4_SMEP: u64 = 1 << 20;
pub const CR4_SMAP: u64 = 1 << 21;

/// CPUID.01H:ECX[30]
pub const CPUID_ECX_RDRAND: u32 = 1 << 30;
/// CPUID.(EAX=7,ECX=0):EBX[7]
pub const CPUID_EBX_SMEP: u32 = 1 << 7;
/// CPUID.(EAX=7,ECX=0):EBX[20]
pub const CPUID_EBX_SMAP: u32 = 1 << 20;
/// CPUID.(EAX=7,ECX=0):ECX[2]
pub const CPUID_ECX_UMIP: u32 = 1 << 2;

static SMAP_LIVE: AtomicBool = AtomicBool::new(false);

/// `stac`/`clac` are #UD when SMAP is not present. Harden sets this.
#[inline]
pub fn smap_live() -> bool {
    SMAP_LIVE.load(Ordering::Acquire)
}

#[inline]
pub fn set_smap_live(on: bool) {
    SMAP_LIVE.store(on, Ordering::Release);
}

/// Set `RFLAGS.AC`. No-op when SMAP is unsupported.
#[inline]
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn stac() {
    if !smap_live() {
        return;
    }
    unsafe { asm!("stac", options(nomem, nostack)) };
}

/// Clear `RFLAGS.AC`. No-op when SMAP is unsupported.
#[inline]
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn clac() {
    if !smap_live() {
        return;
    }
    unsafe { asm!("clac", options(nomem, nostack)) };
}

/// CPUID leaf 7 EBX/ECX, or zeros if the leaf is missing.
#[inline]
pub fn cpuid_leaf7() -> (u32, u32) {
    let (max, _, _, _) = cpuid(0, 0);
    if max < 7 {
        return (0, 0);
    }
    let (_, ebx, ecx, _) = cpuid(7, 0);
    (ebx, ecx)
}

#[inline]
pub fn has_rdrand() -> bool {
    let (_, _, ecx, _) = cpuid(1, 0);
    ecx & CPUID_ECX_RDRAND != 0
}

/// One `RDRAND`. `None` if the feature is missing or the instruction fails.
#[inline]
pub fn rdrand64() -> Option<u64> {
    if !has_rdrand() {
        return None;
    }
    let mut tries = 0u8;
    while tries < 10 {
        let val: u64;
        let ok: u8;
        unsafe {
            asm!(
                "rdrand {val}",
                "setc {ok}",
                val = out(reg) val,
                ok = out(reg_byte) ok,
                options(nomem, nostack),
            );
        }
        if ok != 0 {
            return Some(val);
        }
        tries += 1;
    }
    None
}

#[inline]
pub fn read_cr0() -> u64 {
    let val: u64;
    unsafe { asm!("mov {}, cr0", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// # Safety
/// Caller vouches CR0 bits are valid for this CPU.
#[inline]
pub unsafe fn write_cr0(val: u64) {
    unsafe { asm!("mov cr0, {}", in(reg) val, options(nostack, preserves_flags)) };
}

#[inline]
pub fn read_cr4() -> u64 {
    let val: u64;
    unsafe { asm!("mov {}, cr4", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// # Safety
/// Caller vouches CR4 bits are valid for this CPU.
#[inline]
pub unsafe fn write_cr4(val: u64) {
    unsafe { asm!("mov cr4, {}", in(reg) val, options(nostack, preserves_flags)) };
}

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

#[inline]
pub fn read_rbp() -> u64 {
    let val: u64;
    unsafe { asm!("mov {}, rbp", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// Approximate RIP of the caller-ish (`lea` of the next insn).
#[inline]
pub fn read_rip() -> u64 {
    let val: u64;
    unsafe { asm!("lea {}, [rip]", out(reg) val, options(nomem, nostack, preserves_flags)) };
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

/// Save `RFLAGS.IF`, `cli`, restore on drop. DESIGN §2.3 / ROADMAP §3.5.
/// Nested: each guard saves IF as it found it; only a guard that saw
/// IF=1 restores it, so inner drops do not `sti` while an outer holds.
/// `irq_nest` on the per-CPU area tracks live `InterruptGuard` depth
/// on this CPU. `switch_to` swaps it with the TCB because the guard
/// object stays on the outgoing stack.
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
        crate::per_cpu_init::irq_nest_enter();
        Self {
            restore: rflags & (1 << 9) != 0,
        }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        crate::per_cpu_init::irq_nest_leave();
        if self.restore {
            unsafe { asm!("sti", options(nomem, nostack)) };
        }
    }
}

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a 16-bit write.
#[inline]
#[allow(dead_code)]
pub unsafe fn outw(port: u16, val: u16) {
    unsafe {
        asm!(
            "out dx, ax",
            in("dx") port,
            in("ax") val,
            options(nomem, nostack, preserves_flags)
        )
    };
}

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a 16-bit read.
#[inline]
#[allow(dead_code)]
pub unsafe fn inw(port: u16) -> u16 {
    let val: u16;
    unsafe {
        asm!(
            "in ax, dx",
            out("ax") val,
            in("dx") port,
            options(nomem, nostack, preserves_flags)
        )
    };
    val
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

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a 32-bit read.
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    unsafe {
        asm!(
            "in eax, dx",
            out("eax") val,
            in("dx") port,
            options(nomem, nostack, preserves_flags)
        )
    };
    val
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

/// `CPUID` leaf / subleaf.
#[inline]
pub fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let r = core::arch::x86_64::__cpuid_count(leaf, subleaf);
    (r.eax, r.ebx, r.ecx, r.edx)
}

/// Drain prior stores, including UC MMIO. Not `lfence`: that only
/// serializes loads. SDM Vol. 3A (LAPIC timer TSC-deadline): `MFENCE`
/// or another serializing insn after the LVT timer write, before
/// `IA32_TSC_DEADLINE`. `lfence;rdtsc` / `rdtscp` do not count.
#[inline]
pub fn mfence() {
    unsafe { asm!("mfence", options(nostack, preserves_flags)) };
}

/// `lfence; rdtsc`. DESIGN §6.2: serialize so the read cannot move
/// across a calibration interval boundary.
#[inline]
pub fn lfence_rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!(
            "lfence",
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nostack, nomem, preserves_flags),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// `rdtscp` serializes on its own. The aux CPU number is discarded.
#[inline]
pub fn rdtscp() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!(
            "rdtscp",
            out("eax") lo,
            out("edx") hi,
            out("ecx") _,
            options(nostack, nomem, preserves_flags),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

#[inline]
pub fn sti() {
    unsafe { asm!("sti", options(nomem, nostack, preserves_flags)) };
}

#[inline]
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn cli() {
    unsafe { asm!("cli", options(nomem, nostack, preserves_flags)) };
}

/// One `hlt`. Returns when the next interrupt (or NMI) arrives.
#[inline]
pub fn hlt_once() {
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}

#[inline]
pub fn rflags() -> u64 {
    let v: u64;
    unsafe {
        asm!(
            "pushfq",
            "pop {}",
            out(reg) v,
            options(preserves_flags),
        );
    }
    v
}

#[inline]
pub fn interrupts_enabled() -> bool {
    rflags() & (1 << 9) != 0
}
