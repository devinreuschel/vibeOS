#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! STAR / LSTAR / FMASK / SCE, syscall entry asm, FPU, RSP0/CR3 on switch.
//! ROADMAP §9.1 / §9.3. Same entry as Slice A; stub is the dispatch table.

use core::arch::global_asm;
use core::mem::offset_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use vibeos::addr_space::AddressSpace;
use vibeos::desc::{InterruptFrame, KERNEL_CS, STAR_SYSRET, Tss, USER_CS_RPL, USER_DS_RPL};
use vibeos::fpu;
use vibeos::per_cpu::PerCpu;
use vibeos::syscall::{SyscallFrame, UserRegs};
use vibeos::thread::{Fxsave, Tcb};
use vibeos::vectors;

use crate::arch::gdt;
use crate::cell::BootCell;
use crate::per_cpu_init;
use crate::x86::{
    self, EFER_SCE, FMASK_SYSCALL, IA32_EFER, IA32_FMASK, IA32_FS_BASE, IA32_GS_BASE,
    IA32_KERNEL_GS_BASE, IA32_LSTAR, IA32_STAR,
};

static FPU_READY: AtomicBool = AtomicBool::new(false);
static FPU_TEMPLATE: BootCell<Fxsave> = BootCell::new();

const SCRATCH: usize = offset_of!(PerCpu, syscall_scratch);
const RETVAL: usize = SCRATCH + 8;
const IRET_RIP: usize = SCRATCH + 16;
const IRET_RFLAGS: usize = SCRATCH + 24;
const IRET_RSP: usize = SCRATCH + 32;
const KSP: usize = offset_of!(PerCpu, kernel_rsp0);
const RF_VM: u64 = (1 << 16) | (1 << 17);

global_asm!(
    r#"
    .pushsection .text
    .global vibeos_syscall_entry
    .type vibeos_syscall_entry, @function
    vibeos_syscall_entry:
        // The syscall's GS switch (arch::gs). User RSP is live.
        swapgs
        mov qword ptr gs:[{user_rsp}], rsp
        mov rsp, qword ptr gs:[{ksp}]

        push r15
        push r14
        push r13
        push r12
        push r11
        push r10
        push r9
        push r8
        push rdi
        push rsi
        push rbp
        push rbx
        push rdx
        push rcx
        push rax
        mov rax, qword ptr gs:[{user_rsp}]
        push rax
        // The body runs with IF=1 (DESIGN §2.9 rule 3). Not before the
        // push above: another thread's `syscall` on this CPU overwrites
        // gs:[user_rsp].
        sti

        mov rdi, rsp
        call vibeos_syscall_stub
        // IF is off from here to sysretq or iretq (AGENTS.md rule 2).
        cli
        mov qword ptr gs:[{retval}], rax
        // A non-canonical return RIP reaches neither sysretq nor iretq:
        // the process gets SIGSEGV (AGENTS.md rule 1).
        mov rcx, [rsp + 16]
        mov rax, rcx
        shl rax, 16
        sar rax, 16
        cmp rax, rcx
        jne 20f

        // The FP binding check (DESIGN §7.5); clobbers only registers
        // the exit reloads from the frame or gs:[retval].
        call vibeos_fp_user_return
        mov r11, [rsp + 88]
        test r11, {rf_vm}
        jnz 10f

        mov rax, qword ptr gs:[{retval}]
        mov rbx, [rsp + 32]
        mov rbp, [rsp + 40]
        mov rsi, [rsp + 48]
        mov rdi, [rsp + 56]
        mov rdx, [rsp + 24]
        mov r8,  [rsp + 64]
        mov r9,  [rsp + 72]
        mov r10, [rsp + 80]
        mov r11, [rsp + 88]
        mov r12, [rsp + 96]
        mov r13, [rsp + 104]
        mov r14, [rsp + 112]
        mov r15, [rsp + 120]
        mov rcx, [rsp + 16]
        // Debug builds: IF must be clear (AGENTS.md rule 2). On the
        // kernel stack, before RSP becomes the user's.
        .if {if_check}
        pushfq
        test qword ptr [rsp], 0x200
        lea rsp, [rsp + 8]
        jnz vibeos_exit_if_set
        .endif
        mov rsp, [rsp]
    .global vibeos_syscall_exit_swapgs
    vibeos_syscall_exit_swapgs:
        swapgs
        sysretq

    10:
        mov rax, [rsp + 16]
        mov qword ptr gs:[{iret_rip}], rax
        mov rax, [rsp + 88]
        mov qword ptr gs:[{iret_rflags}], rax
        mov rax, [rsp]
        mov qword ptr gs:[{iret_rsp}], rax
        mov rbx, [rsp + 32]
        mov rbp, [rsp + 40]
        mov rsi, [rsp + 48]
        mov rdi, [rsp + 56]
        mov rdx, [rsp + 24]
        mov r8,  [rsp + 64]
        mov r9,  [rsp + 72]
        mov r10, [rsp + 80]
        mov r12, [rsp + 96]
        mov r13, [rsp + 104]
        mov r14, [rsp + 112]
        mov r15, [rsp + 120]
        add rsp, 128
        push {user_ss}
        mov r11, qword ptr gs:[{iret_rsp}]
        push r11
        mov r11, qword ptr gs:[{iret_rflags}]
        push r11
        push {user_cs}
        mov r11, qword ptr gs:[{iret_rip}]
        push r11
        mov rax, qword ptr gs:[{retval}]
        .if {if_check}
        pushfq
        test qword ptr [rsp], 0x200
        lea rsp, [rsp + 8]
        jnz vibeos_exit_if_set
        .endif
    .global vibeos_syscall_iret_swapgs
    vibeos_syscall_iret_swapgs:
        swapgs
    .global vibeos_syscall_iretq
    vibeos_syscall_iretq:
        iretq

    20:
        mov rdi, rsp
        call {bad_rip}
        ud2

        .if {if_check}
    // A return to ring 3 found IF set (debug builds).
    .global vibeos_exit_if_set
    vibeos_exit_if_set:
        ud2
    .global vibeos_enter_if_set
    vibeos_enter_if_set:
        ud2
        .endif

    .global vibeos_iret_user_full
    .type vibeos_iret_user_full, @function
    vibeos_iret_user_full:
        // IF is off from here to iretq (AGENTS.md rule 2); after
        // `mov gs` nothing may read gs:.
        .if {if_check}
        pushfq
        test qword ptr [rsp], 0x200
        lea rsp, [rsp + 8]
        jnz vibeos_enter_if_set
        .endif
        mov r8, rdx
        mov eax, {user_ss}
        mov ds, ax
        mov es, ax
        mov fs, ax
        mov gs, ax
        mov ecx, {kernel_gs_base}
        mov eax, esi
        mov rdx, rsi
        shr rdx, 32
        wrmsr
        mov ecx, {gs_base}
        xor eax, eax
        xor edx, edx
        wrmsr
        mov ecx, {fs_base}
        mov eax, r8d
        mov rdx, r8
        shr rdx, 32
        wrmsr
        push {user_ss}
        push qword ptr [rdi + {ur_rsp}]
        push qword ptr [rdi + {ur_rflags}]
        push {user_cs}
        push qword ptr [rdi + {ur_rip}]
        mov rax, [rdi + {ur_rax}]
        mov rbx, [rdi + {ur_rbx}]
        mov rcx, [rdi + {ur_rcx}]
        mov rdx, [rdi + {ur_rdx}]
        mov rsi, [rdi + {ur_rsi}]
        mov rbp, [rdi + {ur_rbp}]
        mov r8,  [rdi + {ur_r8}]
        mov r9,  [rdi + {ur_r9}]
        mov r10, [rdi + {ur_r10}]
        mov r11, [rdi + {ur_r11}]
        mov r12, [rdi + {ur_r12}]
        mov r13, [rdi + {ur_r13}]
        mov r14, [rdi + {ur_r14}]
        mov r15, [rdi + {ur_r15}]
        mov rdi, [rdi + {ur_rdi}]
    .global vibeos_iret_user_full_iretq
    vibeos_iret_user_full_iretq:
        iretq
    .popsection
    "#,
    user_rsp = const SCRATCH,
    retval = const RETVAL,
    iret_rip = const IRET_RIP,
    iret_rflags = const IRET_RFLAGS,
    iret_rsp = const IRET_RSP,
    ksp = const KSP,
    rf_vm = const RF_VM,
    if_check = const cfg!(debug_assertions) as u8,
    bad_rip = sym vibeos_syscall_bad_rip,
    kernel_gs_base = const IA32_KERNEL_GS_BASE,
    gs_base = const IA32_GS_BASE,
    fs_base = const IA32_FS_BASE,
    user_cs = const USER_CS_RPL as u64,
    user_ss = const USER_DS_RPL as u64,
    ur_rax = const offset_of!(UserRegs, rax),
    ur_rbx = const offset_of!(UserRegs, rbx),
    ur_rcx = const offset_of!(UserRegs, rcx),
    ur_rdx = const offset_of!(UserRegs, rdx),
    ur_rsi = const offset_of!(UserRegs, rsi),
    ur_rdi = const offset_of!(UserRegs, rdi),
    ur_rbp = const offset_of!(UserRegs, rbp),
    ur_r8 = const offset_of!(UserRegs, r8),
    ur_r9 = const offset_of!(UserRegs, r9),
    ur_r10 = const offset_of!(UserRegs, r10),
    ur_r11 = const offset_of!(UserRegs, r11),
    ur_r12 = const offset_of!(UserRegs, r12),
    ur_r13 = const offset_of!(UserRegs, r13),
    ur_r14 = const offset_of!(UserRegs, r14),
    ur_r15 = const offset_of!(UserRegs, r15),
    ur_rip = const offset_of!(UserRegs, rip),
    ur_rsp = const offset_of!(UserRegs, rsp),
    ur_rflags = const offset_of!(UserRegs, rflags),
);

/// The syscall exit's non-canonical-RIP path: kernel stack and GS, IF=0,
/// the return value already stored. Kills the process with `SIGSEGV`
/// through the ordinary user-fault path.
///
/// # Safety
/// `frame` is the saved syscall frame the exit is returning over.
unsafe extern "C" fn vibeos_syscall_bad_rip(frame: *mut SyscallFrame) -> ! {
    // SAFETY: invariant: `frame` is this thread's saved syscall frame on
    // its kernel stack, which nothing else refers to at the exit;
    // established by `syscall_init::vibeos_syscall_entry`.
    let f = unsafe { &*frame };
    let view = InterruptFrame {
        rip: f.rip,
        cs: u64::from(USER_CS_RPL),
        rflags: f.r11,
        rsp: f.user_rsp,
        ss: u64::from(USER_DS_RPL),
    };
    #[cfg(feature = "kernel_tests")]
    testing::BAD_RIP_KILLS.fetch_add(1, Ordering::Relaxed);
    crate::proc_init::try_user_fault(vectors::GP, &view, 0, None);
    panic!(
        "syscall: non-canonical return RIP {:#x} with no process",
        f.rip
    );
}

unsafe extern "C" {
    fn vibeos_syscall_entry();
    /// Loads the user data selectors, `KERNEL_GS_BASE` = `percpu`,
    /// `GS_BASE` = 0 and `FS_BASE` = `fs_base`, then `iretq`s to `regs`.
    fn vibeos_iret_user_full(regs: *const UserRegs, percpu: u64, fs_base: u64) -> !;
}

/// Write CR0 and CR4 whole (`arch::cpu::init_control_regs`), then program
/// the SYSCALL MSRs and the FPU. Per CPU.
///
/// # Safety
/// GDT loaded, `GS_BASE` is this CPU's `PerCpu`.
pub unsafe fn init_cpu() {
    crate::arch::cpu::init_control_regs();
    let entry = vibeos_syscall_entry as *const () as u64;
    let star = ((STAR_SYSRET as u64) << 48) | ((KERNEL_CS as u64) << 32);
    unsafe {
        x86::wrmsr(IA32_STAR, star);
        x86::wrmsr(IA32_LSTAR, entry);
        x86::wrmsr(IA32_FMASK, FMASK_SYSCALL);
        let efer = x86::rdmsr(IA32_EFER);
        x86::wrmsr(IA32_EFER, efer | EFER_SCE);
    }
    init_fpu();
}

/// BSP: attach the GDT TSS and the dedicated RSP0 stack.
///
/// # Safety
/// GDT loaded, `GS_BASE` is the BSP `PerCpu`.
pub unsafe fn init_bsp() {
    unsafe { init_cpu() };
    per_cpu_init::with_current(|cpu| {
        cpu.tss = gdt::bsp_tss_ptr();
        let top = gdt::bsp_rsp0_top();
        cpu.fallback_rsp0 = top;
        cpu.kernel_rsp0 = top;
        cpu.remote
            .as_cr3
            .store(crate::paging_init::kernel_cr3(), Ordering::Release);
    });
    seed_current_fpu();
}

/// AP: `tables` is this CPU's GDT/TSS. Call after `install_gs`.
///
/// # Safety
/// `tss` is this CPU's live TSS; `rsp0` is its kernel stack top.
pub unsafe fn init_ap(tss: *mut Tss, rsp0: u64) {
    unsafe { init_cpu() };
    per_cpu_init::with_current(|cpu| {
        cpu.tss = tss;
        cpu.fallback_rsp0 = rsp0;
        cpu.kernel_rsp0 = rsp0;
        cpu.remote
            .as_cr3
            .store(crate::paging_init::kernel_cr3(), Ordering::Release);
    });
    seed_current_fpu();
}

/// Reset the x87 unit and, on the first CPU, keep the captured image as
/// the template (`fp_init_template`). `init_control_regs` has already
/// cleared `CR0.EM` and set `CR4.OSFXSR`, which that routine needs.
fn init_fpu() {
    let mut tmpl = Fxsave::empty();
    fp_init_template(&mut tmpl);
    if FPU_TEMPLATE.try_get().is_none() {
        unsafe { FPU_TEMPLATE.set(tmpl) };
        FPU_READY.store(true, Ordering::Release);
    }
}

fn seed_current_fpu() {
    let p = per_cpu_init::current_thread();
    if !p.is_null() {
        unsafe {
            (*p).fpu = fpu_template();
            crate::thread_init::fp_invalidate(&mut *p);
        }
    }
}

// The kernel's only FP and SIMD instructions (`scripts/check_kernel_fp.py`
// allows these three routines and no other).

/// `fninit`, then capture the FP state into `img`.
#[inline(never)]
fn fp_init_template(img: &mut Fxsave) {
    // SAFETY: invariant: CR0.EM is clear and CR4.OSFXSR set, so `fninit`
    // and `fxsave64` execute, and `img` is a 16-byte aligned 512-byte
    // `Fxsave`; established by `arch::cpu::init_control_regs` and
    // `vibeos::thread::Fxsave`.
    unsafe {
        core::arch::asm!("fninit", "fxsave64 [{p}]", p = in(reg) img, options(nostack));
    }
}

/// Save this CPU's FP registers into `tcb.fpu`. Only when the binding says
/// they hold `tcb`'s state, with IF=0 (`switch_fpu`, and a read of the
/// running thread's state inside an `InterruptGuard`, DESIGN §7.5).
#[inline(never)]
pub fn fp_save(tcb: &mut Tcb) {
    // SAFETY: invariant: CR4.OSFXSR is set, and `tcb.fpu` is a 16-byte
    // aligned 512-byte `Fxsave` inside a live TCB; established by
    // `arch::cpu::init_control_regs` and the const assert after
    // `vibeos::thread::Tcb`.
    unsafe {
        core::arch::asm!("fxsave64 [{p}]", p = in(reg) &mut tcb.fpu, options(nostack));
    }
}

/// Load `tcb.fpu` into this CPU's FP registers. Only from
/// `vibeos_fp_user_return`, with IF=0.
#[inline(never)]
fn fp_load(tcb: &Tcb) {
    debug_assert!(
        x86::read_cr0() & x86::CR0_TS == 0,
        "fp_load with CR0.TS set"
    );
    // SAFETY: invariant: CR4.OSFXSR is set, CR0.TS clear, and `tcb.fpu` is
    // a 16-byte aligned FXSAVE image with MXCSR's reserved bits clear (the
    // boot template or a save of this hardware); established by
    // `arch::cpu::init_control_regs`, `syscall_init::fp_init_template` and
    // `syscall_init::fp_save`.
    unsafe {
        core::arch::asm!("fxrstor64 [{p}]", p = in(reg) &tcb.fpu, options(nostack));
    }
}

/// The FP binding check on every return to ring 3 (DESIGN §7.5): the
/// syscall exit, `idt::exit_to_user`, and `enter_user_full`, each with
/// IF=0 on the kernel GS. Loads the current thread's `Tcb.fpu` unless
/// this CPU's registers already hold its state, then binds both fields.
#[unsafe(no_mangle)]
pub extern "C" fn vibeos_fp_user_return() {
    debug_assert!(!x86::interrupts_enabled(), "FP binding check with IF on");
    if !FPU_READY.load(Ordering::Acquire) {
        return;
    }
    per_cpu_init::with_current(|cpu| {
        let t = cpu.current;
        if t.is_null() {
            return;
        }
        // SAFETY: invariant: `cpu.current` is the TCB this CPU runs, live
        // and touched only by this CPU while it runs, and IF=0 keeps it
        // current; established by `thread_init::switch_now`.
        let tcb = unsafe { &mut *t };
        let (me, addr) = (cpu.cpu_id, t as usize);
        if fpu::user_return(cpu.fp_owner, me, addr, tcb.fp_cpu) == fpu::UserReturn::Load {
            fp_load(tcb);
            fpu::bind(&mut cpu.fp_owner, me, addr, &mut tcb.fp_cpu);
        }
    });
}

pub fn fpu_template() -> Fxsave {
    FPU_TEMPLATE
        .try_get()
        .copied()
        .unwrap_or_else(Fxsave::empty)
}

/// Update TSS.RSP0 + `kernel_rsp0` for `tcb`. Every context switch.
/// Caller already holds `&mut PerCpu` (IRQ-off).
pub fn set_rsp0_for(cpu: &mut PerCpu, tcb: &Tcb) {
    let top = match &tcb.stack {
        Some(s) => s.top().as_u64(),
        None => cpu.fallback_rsp0,
    };
    cpu.kernel_rsp0 = top;
    if !cpu.tss.is_null() {
        unsafe { (*cpu.tss).set_rsp0(top) };
    }
}

/// Load `tcb`'s CR3 if it differs. Skip when the next thread shares AS.
///
/// # Safety
/// `cpu` is this CPU's own `PerCpu`, held with IF=0 as `on_switch` holds
/// it. `tcb.as_cr3` is 0 or a root that invariant I128 keeps alive while a
/// TCB names it (`addr_space_init::teardown`).
pub unsafe fn switch_cr3_for(cpu: &mut PerCpu, tcb: &Tcb) -> bool {
    let want = if tcb.as_cr3 == 0 {
        crate::paging_init::kernel_cr3()
    } else {
        tcb.as_cr3
    };
    if cpu.remote.as_cr3.load(Ordering::Relaxed) == want || want == 0 {
        return true;
    }
    unsafe { x86::write_cr3(want) };
    cpu.remote.as_cr3.store(want, Ordering::Release);
    false
}

/// The switch away from `old`: save its FP state if this CPU's registers
/// hold it (the FP binding, DESIGN §7.5). Loads nothing: the next return
/// to ring 3 does.
pub fn switch_fpu(cpu: &mut PerCpu, old: *mut Tcb) {
    if !FPU_READY.load(Ordering::Acquire) || old.is_null() {
        return;
    }
    // SAFETY: invariant: `old` is the TCB this CPU is switching off, live
    // until the switch tail clears its `on_cpu`, with IF=0; established
    // by `thread_init::switch_now`.
    let old = unsafe { &mut *old };
    if fpu::switch_away(
        cpu.fp_owner,
        cpu.cpu_id,
        old as *mut Tcb as usize,
        old.fp_cpu,
    ) == fpu::SwitchAway::Save
    {
        fp_save(old);
    }
}

/// Hardware side of a context switch: FPU, RSP0, CR3. Call before
/// `switch_context`. Caller already holds `&mut PerCpu` (IRQ-off).
pub fn on_switch(cpu: &mut PerCpu, old: *mut Tcb, new: *mut Tcb) {
    switch_fpu(cpu, old);
    if !new.is_null() {
        unsafe {
            set_rsp0_for(cpu, &*new);
            switch_cr3_for(cpu, &*new);
        }
    }
}

/// `iretq` into ring 3 with a full GPR set (fork child / spawned process).
/// Runs `cli` first; `iretq` restores ring 3's IF from `regs.rflags`.
///
/// # Safety
/// `regs.rip`/`regs.rsp` are mapped in the loaded CR3. `regs.rflags`
/// should include the reserved-1 bit.
pub unsafe fn enter_user_full(regs: &UserRegs) -> ! {
    x86::cli();
    #[cfg(feature = "kernel_tests")]
    let regs = &testing::entry_regs(regs);
    vibeos_fp_user_return();
    let ptr = per_cpu_init::current().self_ptr as u64;
    // SAFETY: invariant: IF=0 from the `cli` above to the `iretq`, `ptr`
    // is this CPU's `PerCpu`, and `regs` is a user context the caller
    // vouches for (this fn's contract); established here and by
    // `per_cpu_init::current`.
    unsafe { vibeos_iret_user_full(regs as *const UserRegs, ptr, regs.fs_base) }
}

pub fn star_configured() -> bool {
    let star = x86::rdmsr(IA32_STAR);
    let efer = x86::rdmsr(IA32_EFER);
    let syscall_cs = ((star >> 32) & 0xFFFF) as u16;
    let sysret_cs = ((star >> 48) & 0xFFFF) as u16;
    syscall_cs == KERNEL_CS && sysret_cs == STAR_SYSRET && (efer & EFER_SCE) != 0
}

// --- Slice B: dispatch, early fd1 ---

static TRACE: AtomicBool = AtomicBool::new(false);
static CURRENT_AS: AtomicPtr<AddressSpace> = AtomicPtr::new(ptr::null_mut());
static SYSCALLS: AtomicU64 = AtomicU64::new(0);

#[cfg_attr(feature = "kernel_tests", allow(dead_code))] // tracing / procfs; parked
pub fn set_trace(on: bool) {
    TRACE.store(on, Ordering::Release);
}

#[cfg_attr(feature = "kernel_tests", allow(dead_code))] // tracing / procfs; parked
pub fn trace_enabled() -> bool {
    TRACE.load(Ordering::Acquire)
}

#[cfg_attr(feature = "kernel_tests", allow(dead_code))] // tracing / procfs; parked
pub fn syscall_count() -> u64 {
    let t = per_cpu_init::current_thread();
    if !t.is_null() {
        unsafe { (*t).syscall_count }
    } else {
        SYSCALLS.load(Ordering::Relaxed)
    }
}

fn current_as() -> Option<&'static AddressSpace> {
    let p = CURRENT_AS.load(Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        Some(unsafe { &*p })
    }
}

pub fn peek_user_as() -> Option<&'static AddressSpace> {
    current_as()
}

pub fn set_user_as(space: &AddressSpace) {
    CURRENT_AS.store(
        space as *const AddressSpace as *mut AddressSpace,
        Ordering::Release,
    );
}

pub fn clear_user_as() {
    CURRENT_AS.store(ptr::null_mut(), Ordering::Release);
}

fn bump_counter() {
    SYSCALLS.fetch_add(1, Ordering::Relaxed);
    let t = per_cpu_init::current_thread();
    if !t.is_null() {
        unsafe { (*t).syscall_count = (*t).syscall_count.wrapping_add(1) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn vibeos_syscall_stub(frame: *mut SyscallFrame) -> i64 {
    bump_counter();
    let r = crate::proc_init::syscall(frame);
    #[cfg(feature = "kernel_tests")]
    testing::on_exit(frame);
    r
}

pub fn dispatch(nr: u64, args: [u64; 6]) -> i64 {
    crate::proc_init::dispatch(nr, args)
}

/// In-guest test hooks. `kernel_tests` only (AGENTS.md rule 9).
#[cfg(feature = "kernel_tests")]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use vibeos::paging::USER_END;
    use vibeos::syscall::{SyscallFrame, UserRegs};

    /// Syscall number plus one whose next exit gets a non-canonical RIP;
    /// 0 for none.
    static ARMED_NR: AtomicU64 = AtomicU64::new(0);
    static ARMED_ENTRY: AtomicBool = AtomicBool::new(false);
    pub(super) static BAD_RIP_KILLS: AtomicU64 = AtomicU64::new(0);

    /// The next exit of syscall `nr` from a process returns to a
    /// non-canonical RIP.
    pub(crate) fn arm_noncanonical_rip(nr: u64) {
        ARMED_NR.store(nr.wrapping_add(1), Ordering::Release);
    }

    /// Undo [`arm_noncanonical_rip`] and [`arm_noncanonical_entry`].
    pub(crate) fn disarm_noncanonical() {
        ARMED_NR.store(0, Ordering::Release);
        ARMED_ENTRY.store(false, Ordering::Release);
    }

    /// The next `enter_user_full` enters at a non-canonical RIP.
    pub(crate) fn arm_noncanonical_entry() {
        ARMED_ENTRY.store(true, Ordering::Release);
    }

    /// Processes the syscall exit's non-canonical path has killed.
    pub(crate) fn bad_rip_kills() -> u64 {
        BAD_RIP_KILLS.load(Ordering::Relaxed)
    }

    pub(super) fn on_exit(frame: *mut SyscallFrame) {
        let t = crate::per_cpu_init::current_thread();
        // SAFETY: invariant: a non-null current thread is this CPU's live
        // TCB while it runs; established by `per_cpu_init::set_current_thread`.
        if t.is_null() || unsafe { (*t).pid } == 0 {
            return;
        }
        // SAFETY: invariant: `frame` is this thread's saved syscall frame
        // on its kernel stack, which the dispatcher has returned from;
        // established by `syscall_init::vibeos_syscall_entry`.
        let f = unsafe { &mut *frame };
        let armed = f.nr.wrapping_add(1);
        if ARMED_NR
            .compare_exchange(armed, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            f.rip = USER_END;
        }
    }

    pub(super) fn entry_regs(regs: &UserRegs) -> UserRegs {
        let mut r = *regs;
        if ARMED_ENTRY.swap(false, Ordering::AcqRel) {
            r.rip = USER_END;
        }
        r
    }
}
