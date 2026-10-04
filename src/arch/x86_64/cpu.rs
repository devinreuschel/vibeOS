//! Tiny x86_64 primitives. Everything here is intentionally small.
//!
//! Per-CPU control registers: one routine writes CR0 and CR4 whole on every
//! CPU from values the BSP computes once from CPUID (DESIGN §5.1, §11.4).

use core::arch::asm;
use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use vibeos::log::Level;

use crate::x86;

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a byte write.
#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags))
    };
}

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a byte read.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
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
    // SAFETY: `mov r, cr3` only reads CR3 into a register, as its `options` say; established here.
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
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
    unsafe { asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags)) };
}

/// `invlpg` for a single virtual address. Cheap enough that every leaf
/// edit calls it; DESIGN §4.3 requires it after any single-PTE change.
#[inline]
pub fn invlpg(va: u64) {
    // SAFETY: `invlpg` only drops TLB entries for one page, which never changes
    // what a later access finds, only how long it takes; established here.
    unsafe { asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags)) };
}

/// Read an MSR by index.
#[inline]
pub fn rdmsr(msr: u32) -> u64 {
    let hi: u32;
    let lo: u32;
    // SAFETY: `rdmsr` only reads an MSR into registers; an index the CPU lacks
    // raises `#GP`, which halts the kernel rather than touching memory; established here.
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
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
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

/// The SYSENTER target: CS (SS is CS + 8), RSP and RIP (SDM Vol. 4).
pub const IA32_SYSENTER_CS: u32 = 0x174;
pub const IA32_SYSENTER_ESP: u32 = 0x175;
pub const IA32_SYSENTER_EIP: u32 = 0x176;
pub const IA32_EFER: u32 = 0xC000_0080;
pub const EFER_NXE: u64 = 1 << 11;
pub const EFER_SCE: u64 = 1 << 0;
/// AMD fast FXSAVE: `fxsave64` and `fxrstor64` at CPL 0 in long mode skip
/// XMM0-15 (APM Vol. 2 §3.1.7).
pub const EFER_FFXSR: u64 = 1 << 14;
pub const IA32_STAR: u32 = 0xC000_0081;
pub const IA32_LSTAR: u32 = 0xC000_0082;
pub const IA32_FMASK: u32 = 0xC000_0084;
pub const IA32_FS_BASE: u32 = 0xC000_0100;
pub const IA32_GS_BASE: u32 = 0xC000_0101;
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// CPUID faulting (Intel's VT FlexMigration application note):
/// `MSR_PLATFORM_INFO` bit 31 enumerates it, and bit 0 of
/// `MSR_MISC_FEATURES_ENABLES` makes `cpuid` at CPL 3 raise `#GP`.
pub const MSR_PLATFORM_INFO: u32 = 0xCE;
pub const MSR_MISC_FEATURES_ENABLES: u32 = 0x140;
pub const PLATFORM_INFO_CPUID_FAULTING: u64 = 1 << 31;
pub const MISC_FEATURES_CPUID_FAULTING: u64 = 1 << 0;

/// TF|IF|DF|IOPL|NT|AC. Cleared on `syscall`.
pub const FMASK_SYSCALL: u64 = 0x47700;

pub const CR0_PE: u64 = 1 << 0;
pub const CR0_MP: u64 = 1 << 1;
pub const CR0_TS: u64 = 1 << 3;
pub const CR0_ET: u64 = 1 << 4;
pub const CR0_NE: u64 = 1 << 5;
pub const CR0_WP: u64 = 1 << 16;
pub const CR0_AM: u64 = 1 << 18;
pub const CR0_PG: u64 = 1 << 31;
pub const CR4_TSD: u64 = 1 << 2;
pub const CR4_PAE: u64 = 1 << 5;
pub const CR4_MCE: u64 = 1 << 6;
pub const CR4_PGE: u64 = 1 << 7;
pub const CR4_PCE: u64 = 1 << 8;
pub const CR4_OSFXSR: u64 = 1 << 9;
pub const CR4_OSXMMEXCPT: u64 = 1 << 10;
pub const CR4_UMIP: u64 = 1 << 11;
pub const CR4_LA57: u64 = 1 << 12;
pub const CR4_OSXSAVE: u64 = 1 << 18;
pub const CR4_SMEP: u64 = 1 << 20;
pub const CR4_SMAP: u64 = 1 << 21;
pub const CR4_PKE: u64 = 1 << 22;

/// CPUID.01H:ECX[20]
pub const CPUID_ECX_SSE42: u32 = 1 << 20;
/// CPUID.01H:ECX[30]
pub const CPUID_ECX_RDRAND: u32 = 1 << 30;
/// CPUID.01H:EDX[7]
pub const CPUID_EDX_MCE: u32 = 1 << 7;
/// CPUID.01H:EDX[13]
pub const CPUID_EDX_PGE: u32 = 1 << 13;
/// CPUID.(EAX=7,ECX=0):EBX[7]
pub const CPUID_EBX_SMEP: u32 = 1 << 7;
/// CPUID.(EAX=7,ECX=0):EBX[20]
pub const CPUID_EBX_SMAP: u32 = 1 << 20;
/// CPUID.(EAX=7,ECX=0):ECX[2]
pub const CPUID_ECX_UMIP: u32 = 1 << 2;

static SMAP_LIVE: AtomicBool = AtomicBool::new(false);

/// `stac`/`clac` are #UD when SMAP is not present. `arch::cpu::init_control_regs` sets this.
#[inline]
pub fn smap_live() -> bool {
    SMAP_LIVE.load(Ordering::Acquire)
}

#[inline]
pub fn set_smap_live(on: bool) {
    SMAP_LIVE.store(on, Ordering::Release);
}

/// Clear `RFLAGS.AC`. No-op when SMAP is unsupported. Setting it is the
/// accessor module's alone (`arch::x86_64::uaccess`, ROADMAP §10.6).
#[inline]
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §9.1's AC-clear helper; the accessor module's test window calls it"
    )
)]
pub fn clac() {
    if !smap_live() {
        return;
    }
    // SAFETY: `clac` only changes RFLAGS.AC, and `smap_live` is set only
    // once CR4.SMAP is on, so the instruction is defined; established
    // at `arch::x86_64::cpu::init_control_regs`.
    unsafe { asm!("clac", options(nostack)) };
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
        // SAFETY: `rdrand` and `setc` only write the two named registers and flags; established here.
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
    // SAFETY: `mov r, cr0` only reads CR0 into a register, as its `options`
    // say; established here.
    unsafe { asm!("mov {}, cr0", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// # Safety
/// Caller vouches CR0 bits are valid for this CPU.
#[inline]
pub unsafe fn write_cr0(val: u64) {
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
    unsafe { asm!("mov cr0, {}", in(reg) val, options(nostack, preserves_flags)) };
}

#[inline]
pub fn read_cr4() -> u64 {
    let val: u64;
    // SAFETY: `mov r, cr4` only reads CR4 into a register, as its `options` say; established here.
    unsafe { asm!("mov {}, cr4", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// # Safety
/// Caller vouches CR4 bits are valid for this CPU.
#[inline]
pub unsafe fn write_cr4(val: u64) {
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
    unsafe { asm!("mov cr4, {}", in(reg) val, options(nostack, preserves_flags)) };
}

/// Read `rsp`. Used by paging bring-up to find which top-level PML4
/// entry covers Limine's boot stack, so the switch to our own PML4
/// survives the following `mov cr3`.
#[inline]
pub fn read_rsp() -> u64 {
    let val: u64;
    // SAFETY: `mov r, rsp` only copies RSP into a register; established here.
    unsafe { asm!("mov {}, rsp", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

#[inline]
pub fn read_rbp() -> u64 {
    let val: u64;
    // SAFETY: `mov r, rbp` only copies RBP into a register; established here.
    unsafe { asm!("mov {}, rbp", out(reg) val, options(nomem, nostack, preserves_flags)) };
    val
}

/// Approximate RIP of the caller-ish (`lea` of the next insn).
#[inline]
pub fn read_rip() -> u64 {
    let val: u64;
    // SAFETY: `lea r, [rip]` only computes an address into a register; established here.
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
        // SAFETY: `cli; hlt` stops this CPU with interrupts off and touches no memory; established here.
        unsafe { asm!("cli; hlt", options(nostack)) };
    }
}

// Per-CPU hooks (DESIGN §1.2): the per-CPU module's `init_bsp` installs them
// before it marks the per-CPU area live. Unset, the nest hooks do nothing and
// `cpu_index` returns `None`, as before the area exists.
static NEST_ENTER: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());
static NEST_LEAVE: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());
static CPU_INDEX: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the per-CPU hooks: `InterruptGuard`'s nesting count and this
/// CPU's index. The per-CPU module's `init_bsp` calls it once, on the BSP.
pub fn set_per_cpu_hooks(nest_enter: fn(), nest_leave: fn(), cpu_index: fn() -> Option<u32>) {
    // Release: pairs with the Acquire loads in `run_hook` and `cpu_index`,
    // so a CPU that sees a hook sees what `init_bsp` wrote before it.
    NEST_ENTER.store(nest_enter as *mut (), Ordering::Release);
    NEST_LEAVE.store(nest_leave as *mut (), Ordering::Release);
    CPU_INDEX.store(cpu_index as *mut (), Ordering::Release);
}

/// Run a `fn()` hook of `set_per_cpu_hooks`, or nothing when it is unset.
#[inline]
fn run_hook(hook: &AtomicPtr<()>) {
    // Acquire: pairs with the Release stores in `set_per_cpu_hooks`.
    let p = hook.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `NEST_ENTER` or `NEST_LEAVE` holds a
    // `fn()`; established by `arch::cpu::set_per_cpu_hooks`, their only
    // store.
    let f = unsafe { core::mem::transmute::<*mut (), fn()>(p) };
    f();
}

/// This CPU's index (`cpu_id`), or `None` before the per-CPU area is live.
pub fn cpu_index() -> Option<u32> {
    // Acquire: pairs with the Release store in `set_per_cpu_hooks`.
    let p = CPU_INDEX.load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: invariant: a non-null `CPU_INDEX` holds a
    // `fn() -> Option<u32>`; established by `arch::cpu::set_per_cpu_hooks`,
    // its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn() -> Option<u32>>(p) };
    f()
}

/// Save `RFLAGS.IF`, `cli`, restore on drop. DESIGN §2.3 / ROADMAP §3.5.
/// Nested: each guard saves IF as it found it; only a guard that saw
/// IF=1 restores it, so inner drops do not `sti` while an outer holds.
/// `irq_nest` on the per-CPU area tracks live `InterruptGuard` depth
/// on this CPU. `switch_to` swaps it with the TCB because the guard
/// object stays on the outgoing stack.
///
/// `!Send`, as `std::sync::MutexGuard` is: the IF state it restores
/// belongs to the CPU that entered it (ROADMAP §10.3, F038).
pub struct InterruptGuard {
    restore: bool,
    _not_send: PhantomData<*const ()>,
}

impl InterruptGuard {
    /// `#[track_caller]`: the irqoff tracer names the caller as the site
    /// that turned IF off.
    #[track_caller]
    pub fn enter() -> Self {
        let rflags: u64;
        // SAFETY: `pushfq; pop; cli` reads RFLAGS through one stack slot it
        // pops again and clears IF; established here.
        unsafe {
            asm!(
                "pushfq",
                "pop {0}",
                "cli",
                out(reg) rflags,
            );
        }
        run_hook(&NEST_ENTER);
        let restore = rflags & (1 << 9) != 0;
        if restore {
            crate::sched::irqoff::off(crate::sched::irqoff::Site::caller(
                core::panic::Location::caller(),
            ));
        }
        Self {
            restore,
            _not_send: PhantomData,
        }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        run_hook(&NEST_LEAVE);
        if self.restore {
            crate::sched::irqoff::on();
            // SAFETY: IF was 1 when this guard entered (here), so turning it
            // back on restores the state its holder found.
            unsafe { asm!("sti", options(nostack)) };
        }
    }
}

/// # Safety
/// Caller vouches that `port` is a valid I/O port for a 16-bit write.
#[inline]
pub unsafe fn outw(port: u16, val: u16) {
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
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
/// Caller vouches that `port` is a valid I/O port for a 32-bit write.
#[inline]
pub unsafe fn outl(port: u16, val: u32) {
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
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
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
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
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
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
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
    unsafe {
        asm!(
            "lidt [{}]",
            in(reg) ptr,
            options(readonly, nostack, preserves_flags)
        )
    };
}

/// Load DS, ES, SS, FS and GS with `sel`. `mov gs` zeroes `GS_BASE`.
///
/// # Safety
/// `sel` is a valid data selector in the loaded GDT, and the caller writes
/// `GS_BASE` back before anything reads a `gs:` operand, with IF=0 in
/// between when it runs at CPL 0.
#[inline]
pub unsafe fn load_data_segs(sel: u16) {
    // SAFETY: this fn's `# Safety` (here) is the instructions' one
    // requirement; their `options` state everything else they touch.
    unsafe {
        asm!(
            "mov ds, {0:x}",
            "mov es, {0:x}",
            "mov ss, {0:x}",
            "mov fs, {0:x}",
            "mov gs, {0:x}",
            in(reg) sel,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
/// `sel` must index an available 64-bit TSS descriptor in the current GDT.
#[inline]
pub unsafe fn ltr(sel: u16) {
    // SAFETY: this fn's `# Safety` (here) is the instruction's one
    // requirement; its `options` state everything else it touches.
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
    // SAFETY: `mfence` only orders this CPU's memory accesses; established here.
    unsafe { asm!("mfence", options(nostack, preserves_flags)) };
}

/// `lfence; rdtsc`. DESIGN §6.2: serialize so the read cannot move
/// across a calibration interval boundary.
#[inline]
pub fn lfence_rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: `lfence; rdtsc` only orders loads and reads the TSC into EDX:EAX; established here.
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
    // SAFETY: `rdtscp` only reads the TSC and `TSC_AUX` into the named registers; established here.
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
    crate::sched::irqoff::on();
    // SAFETY: `sti` only sets IF; every interrupt then enters through the IDT's
    // stubs, which preserve the interrupted state; established here.
    unsafe { asm!("sti", options(nostack, preserves_flags)) };
}

#[inline]
#[track_caller]
pub fn cli() {
    // Only an `irqoff` build reads IF first; the tracer stamps a 1 to 0.
    let was_on = cfg!(feature = "irqoff") && interrupts_enabled();
    // SAFETY: `cli` only clears IF; established here.
    unsafe { asm!("cli", options(nostack, preserves_flags)) };
    if was_on {
        crate::sched::irqoff::off_here();
    }
}

/// One `hlt`. Returns when the next interrupt (or NMI) arrives.
#[inline]
pub fn hlt_once() {
    // SAFETY: `hlt` waits for the next interrupt, which returns through the IDT's
    // stubs with the interrupted state intact; established here.
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}

#[inline]
pub fn rflags() -> u64 {
    let v: u64;
    // SAFETY: `pushfq; pop` reads RFLAGS through one stack slot it pops again; established here.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    pub smep: bool,
    pub smap: bool,
    pub umip: bool,
    pub mce: bool,
    pub pge: bool,
}

pub fn cpuid_features() -> Features {
    let (ebx, ecx) = x86::cpuid_leaf7();
    let (_, _, _, edx1) = x86::cpuid(1, 0);
    Features {
        smep: ebx & CPUID_EBX_SMEP != 0,
        smap: ebx & CPUID_EBX_SMAP != 0,
        umip: ecx & CPUID_ECX_UMIP != 0,
        mce: edx1 & CPUID_EDX_MCE != 0,
        pge: edx1 & CPUID_EDX_PGE != 0,
    }
}

/// CPUID.0 EBX, EDX, ECX: "GenuineIntel".
const VENDOR_INTEL: [u32; 3] = [0x756E_6547, 0x4965_6E69, 0x6C65_746E];

/// Whether this CPU has CPUID faulting. No CPUID bit says
/// `MSR_PLATFORM_INFO` exists and a `rdmsr` of a missing MSR raises `#GP`,
/// which halts the kernel, so only an Intel CPU with SSE4.2 reads it:
/// Nehalem, the first with SSE4.2, and every Intel core since have it.
pub fn cpuid_faulting() -> bool {
    let (_, ebx, ecx, edx) = cpuid(0, 0);
    let (_, _, ecx1, _) = cpuid(1, 0);
    [ebx, edx, ecx] == VENDOR_INTEL
        && ecx1 & CPUID_ECX_SSE42 != 0
        && rdmsr(MSR_PLATFORM_INFO) & PLATFORM_INFO_CPUID_FAULTING != 0
}

/// The CR0 and CR4 every CPU runs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlRegs {
    pub cr0: u64,
    pub cr4: u64,
}

// The values `init_control_regs` writes, stored by its first call (the
// BSP's, before `smp: done`). CR0 is 0 until then: the stored CR0 always
// has PE set.
static CONTROL_CR0: AtomicU64 = AtomicU64::new(0);
static CONTROL_CR4: AtomicU64 = AtomicU64::new(0);

pub(crate) fn stored_control_regs() -> Option<ControlRegs> {
    // Acquire: pairs with the Release store of CR0 in `init_control_regs`,
    // so CR4 is the value stored before it.
    let cr0 = CONTROL_CR0.load(Ordering::Acquire);
    if cr0 == 0 {
        return None;
    }
    Some(ControlRegs {
        cr0,
        cr4: CONTROL_CR4.load(Ordering::Relaxed),
    })
}

/// CR0: PE, MP, ET, NE, WP, AM, PG, so EM, TS, CD and NW are clear. CR4:
/// PAE, OSFXSR, OSXMMEXCPT, and MCE, PGE, SMEP, SMAP and UMIP where CPUID
/// reports them; every other bit is clear, TSD and PCE (DESIGN §11.4) and
/// OSXSAVE and PKE (ROADMAP §11.1) included.
fn compute() -> ControlRegs {
    let f = cpuid_features();
    let cr0 = CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_WP | CR0_AM | CR0_PG;
    let mut cr4 = CR4_PAE | CR4_OSFXSR | CR4_OSXMMEXCPT;
    for (on, bit) in [
        (f.mce, CR4_MCE),
        (f.pge, CR4_PGE),
        (f.smep, CR4_SMEP),
        (f.smap, CR4_SMAP),
        (f.umip, CR4_UMIP),
    ] {
        if on {
            cr4 |= bit;
        }
    }
    ControlRegs { cr0, cr4 }
}

/// Write this CPU's CR0 and CR4 whole, and turn CPUID faulting off where
/// the CPU has it. Every CPU calls it from `syscall_init::init_cpu`: the BSP
/// through `init_bsp`, each AP through `init_ap`. The BSP's call, the first,
/// computes the values before `smp: done`. This is a CPU's last CR4 store:
/// the AP trampoline's comes before it, and no TLB flush writes CR4.
pub fn init_control_regs() {
    // A whole CR4 write that clears LA57 under 5-level paging raises #GP.
    // The kernel asks Limine for no 5-level paging, and the AP trampoline
    // builds 4-level mode, so no CPU has it set.
    assert!(x86::read_cr4() & CR4_LA57 == 0, "CR4.LA57 set");
    let stored = stored_control_regs();
    let first = stored.is_none();
    let regs = match stored {
        Some(r) => r,
        None => {
            // One writer: the BSP's `syscall_init::init_bsp` makes the
            // first call before `smp_init::init` starts any AP.
            let r = compute();
            CONTROL_CR4.store(r.cr4, Ordering::Relaxed);
            // Release: pairs with the Acquire load in `stored_control_regs`.
            CONTROL_CR0.store(r.cr0, Ordering::Release);
            r
        }
    };
    // SAFETY: CR0 keeps PE and PG and CR4 keeps PAE, which long mode
    // requires, and every other bit is one CPUID reports or every x86-64
    // CPU has; established by `arch::cpu::compute`.
    unsafe {
        x86::write_cr0(regs.cr0);
        x86::write_cr4(regs.cr4);
    }
    // The FP switch saves only the 512-byte FXSAVE image (F130), and ring 3
    // runs `rdtsc` but not `rdpmc` (DESIGN §11.4).
    assert!(
        x86::read_cr4() & (CR4_OSXSAVE | CR4_PKE | CR4_TSD | CR4_PCE) == 0,
        "CR4.OSXSAVE, PKE, TSD or PCE set"
    );
    x86::set_smap_live(regs.cr4 & CR4_SMAP != 0);
    let fault = cpuid_faulting();
    if fault {
        let misc = x86::rdmsr(MSR_MISC_FEATURES_ENABLES) & !MISC_FEATURES_CPUID_FAULTING;
        // SAFETY: the MSR exists where `MSR_PLATFORM_INFO` enumerates CPUID
        // faulting, and clearing bit 0 only lets ring 3 run `cpuid`; every
        // other bit is written back as read; established by
        // `arch::cpu::cpuid_faulting`.
        unsafe { x86::wrmsr(MSR_MISC_FEATURES_ENABLES, misc) };
    }
    if first {
        let smep = u8::from(regs.cr4 & CR4_SMEP != 0);
        let smap = u8::from(regs.cr4 & CR4_SMAP != 0);
        let umip = u8::from(regs.cr4 & CR4_UMIP != 0);
        let wp = u8::from(regs.cr0 & CR0_WP != 0);
        let fault = u8::from(fault);
        crate::klog!(
            Level::Info,
            "vibeOS: cpu: cr0={:#x} cr4={:#x} smep={smep} smap={smap} umip={umip} wp={wp} \
             cpuid_fault={fault}",
            regs.cr0,
            regs.cr4
        );
    }
}

/// The isa-debug-exit port the harness gives every test boot
/// (`-device isa-debug-exit,iobase=0xf4`).
#[cfg(feature = "kernel_tests")]
const ISA_DEBUG_EXIT: u16 = 0xF4;

/// End the QEMU run with exit status `(code << 1) | 1` through
/// isa-debug-exit, and halt if no such device took the write: the one
/// isa-debug-exit writer, for the test build's verdict.
#[cfg(feature = "kernel_tests")]
pub fn qemu_exit(code: u32) -> ! {
    // SAFETY: invariant: port 0xF4 is the harness's isa-debug-exit device,
    // whose write ends the VM, and no device on a machine without it
    // answers there; established by the `-device isa-debug-exit` the
    // harness passes, which `arch::x86_64::cpu::ISA_DEBUG_EXIT` names.
    unsafe { outl(ISA_DEBUG_EXIT, code) };
    halt();
}

/// One word from the hardware RNG (`RDRAND`), or `None` when the CPU has
/// none or it failed ten times.
#[inline]
pub fn hw_rng64() -> Option<u64> {
    rdrand64()
}

/// The user TLS register (`FS_BASE`) this CPU holds now.
#[inline]
pub fn user_tls() -> u64 {
    rdmsr(IA32_FS_BASE)
}

/// Load the user TLS register (`FS_BASE`) with `v`.
///
/// # Safety
/// `v` is a canonical address: a non-canonical `FS_BASE` write faults.
#[inline]
pub unsafe fn set_user_tls(v: u64) {
    // SAFETY: `FS_BASE` is an architectural MSR, and `v` is canonical (this
    // fn's `# Safety` contract, established here); ring 0 never reads `fs:`.
    unsafe { wrmsr(IA32_FS_BASE, v) };
}

/// This thread's stack pointer now.
#[inline]
pub fn stack_pointer() -> u64 {
    read_rsp()
}
