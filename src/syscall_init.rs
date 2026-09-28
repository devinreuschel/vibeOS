#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! STAR / LSTAR / FMASK / SCE, syscall entry asm, FPU, RSP0/CR3 on switch.
//! ROADMAP §9.1 / §9.3. Same entry as Slice A; stub is the dispatch table.

use core::arch::global_asm;
use core::mem::{offset_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use vibeos::addr_space::AddressSpace;
use vibeos::desc::{KERNEL_CS, STAR_SYSRET, Tss, USER_CS_RPL, USER_DS_RPL};
use vibeos::fpu;
use vibeos::per_cpu::PerCpu;
use vibeos::syscall::UserFrame;
use vibeos::thread::{Fxsave, Tcb};
use vibeos::trap::x86_64::sysret_ok;
use vibeos::vectors;

use crate::arch::gdt;
use crate::arch::idt::TrapFrame;
use crate::cell::BootCell;
use crate::per_cpu_init;
use crate::x86::{
    self, EFER_SCE, FMASK_SYSCALL, IA32_EFER, IA32_FMASK, IA32_FS_BASE, IA32_GS_BASE,
    IA32_KERNEL_GS_BASE, IA32_LSTAR, IA32_STAR,
};

static FPU_READY: AtomicBool = AtomicBool::new(false);
static FPU_TEMPLATE: BootCell<Fxsave> = BootCell::new();

const SCRATCH: usize = offset_of!(PerCpu, syscall_scratch);
const KSP: usize = offset_of!(PerCpu, kernel_rsp0);
/// The user frame sits one pad word above RSP from the entry's `call` to
/// the exit's pops (`vibeos_syscall_return` is entered at RSP = frame - 8).
const PAD: usize = 8;
const F_RAX: usize = PAD + offset_of!(UserFrame, rax);
const F_RIP: usize = PAD + offset_of!(UserFrame, rip);
/// From `orig_rax`, where the 15 GPR pops leave RSP, to the `rsp` slot.
const ORIG_TO_RSP: usize = offset_of!(UserFrame, rsp) - offset_of!(UserFrame, orig_rax);
/// The value Linux shows in the `rax` slot at a syscall-entry stop.
const ENOSYS_RET: i64 = -(vibeos::syscall::ENOSYS as i64);

const _: () = {
    // 21 words and the pad: RSP is 16-byte aligned at the `call`.
    assert!(size_of::<UserFrame>() == 168);
    assert!((size_of::<UserFrame>() + PAD).is_multiple_of(16));
};

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

        // The user frame (vibeos::trap::x86_64::UserFrame), top down:
        // RCX is the return RIP and R11 the user RFLAGS.
        push {user_ss}
        push qword ptr gs:[{user_rsp}]
        push r11
        push {user_cs}
        push rcx
        push rax
        push rdi
        push rsi
        push rdx
        push rcx
        push {enosys}
        push r8
        push r9
        push r10
        push r11
        push rbx
        push rbp
        push r12
        push r13
        push r14
        push r15
        sub rsp, 8
        // The body runs with IF=1 (DESIGN §2.9 rule 3). Not before the
        // pushes above: another thread's `syscall` on this CPU overwrites
        // gs:[user_rsp].
        sti

        lea rdi, [rsp + {pad}]
        call vibeos_syscall_stub
        // IF is off from here to sysretq or iretq (AGENTS.md rule 2).
        cli
        mov [rsp + {f_rax}], rax

    // The return to ring 3 over the user frame at RSP + 8: IF=0, kernel
    // GS. `first_return` enters here too.
    .global vibeos_syscall_return
    vibeos_syscall_return:
        // A non-canonical return RIP reaches neither sysretq nor iretq:
        // the process gets SIGSEGV (AGENTS.md rule 1).
        mov rcx, [rsp + {f_rip}]
        mov rax, rcx
        shl rax, 16
        sar rax, 16
        cmp rax, rcx
        jne 20f

        // The FP binding check (DESIGN §7.5). Both calls clobber only
        // registers the exit reloads from the frame.
        call vibeos_fp_user_return
        lea rdi, [rsp + {pad}]
        call vibeos_sysret_ok
        test al, al
        jz 10f

        add rsp, {pad}
        pop r15
        pop r14
        pop r13
        pop r12
        pop rbp
        pop rbx
        pop r11
        pop r10
        pop r9
        pop r8
        pop rax
        pop rcx
        pop rdx
        pop rsi
        pop rdi
        // Debug builds: IF must be clear (AGENTS.md rule 2). On the
        // kernel stack, before RSP becomes the user's.
        .if {if_check}
        pushfq
        test qword ptr [rsp], 0x200
        lea rsp, [rsp + 8]
        jnz vibeos_exit_if_set
        .endif
        mov rsp, [rsp + {orig_to_rsp}]
    .global vibeos_syscall_exit_swapgs
    vibeos_syscall_exit_swapgs:
        swapgs
        sysretq

    // All 15 GPRs and the frame's own RIP, CS, RFLAGS, RSP and SS.
    10:
        add rsp, {pad}
        pop r15
        pop r14
        pop r13
        pop r12
        pop rbp
        pop rbx
        pop r11
        pop r10
        pop r9
        pop r8
        pop rax
        pop rcx
        pop rdx
        pop rsi
        pop rdi
        add rsp, 8
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
        lea rdi, [rsp + {pad}]
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

    .popsection
    "#,
    user_rsp = const SCRATCH,
    ksp = const KSP,
    pad = const PAD,
    f_rax = const F_RAX,
    f_rip = const F_RIP,
    orig_to_rsp = const ORIG_TO_RSP,
    enosys = const ENOSYS_RET,
    if_check = const cfg!(debug_assertions) as u8,
    bad_rip = sym vibeos_syscall_bad_rip,
    user_cs = const USER_CS_RPL as u64,
    user_ss = const USER_DS_RPL as u64,
);

/// The syscall exit's non-canonical-RIP path: kernel stack and GS, IF=0,
/// the return value already stored. Kills the process with `SIGSEGV`
/// through the ordinary user-fault path.
///
/// # Safety
/// `frame` is the user frame the exit is returning over.
unsafe extern "C" fn vibeos_syscall_bad_rip(frame: *mut UserFrame) -> ! {
    // SAFETY: invariant I25: `frame` is this thread's user frame at the top
    // of its kernel stack, which nothing else refers to at the exit;
    // established by `syscall_init::vibeos_syscall_entry` or
    // `thread_init::spawn_user`.
    let f = unsafe { &*frame };
    #[cfg(feature = "kernel_tests")]
    testing::BAD_RIP_KILLS.fetch_add(1, Ordering::Relaxed);
    crate::proc_init::try_user_fault(&TrapFrame::for_user(vectors::GP, 0, f));
    panic!(
        "syscall: non-canonical return RIP {:#x} with no process",
        f.rip
    );
}

/// The exit's choice between `sysretq` and `iretq` (Linux's rule,
/// `trap::x86_64::sysret_ok`).
///
/// # Safety
/// `f` is the user frame the exit is returning over.
#[unsafe(no_mangle)]
unsafe extern "C" fn vibeos_sysret_ok(f: *const UserFrame) -> bool {
    // SAFETY: invariant I25: `f` is this thread's user frame at the top of
    // its kernel stack; established by `syscall_init::vibeos_syscall_entry`
    // or `thread_init::spawn_user`.
    sysret_ok(unsafe { &*f })
}

unsafe extern "C" {
    fn vibeos_syscall_entry();
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
/// syscall exit (which `first_return` enters too) and `idt::exit_to_user`, each with
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

/// The running thread's x87 status word, x87 control word, and MXCSR,
/// read under the FP binding's read rule (DESIGN §7.5, C-FPBIND): inside
/// an `InterruptGuard`, this CPU's registers are saved into `Tcb.fpu`
/// first when they hold its state. `None` before the FPU is set up or
/// with no current thread.
pub fn current_fp_words() -> Option<(u32, u32, u32)> {
    if !FPU_READY.load(Ordering::Acquire) {
        return None;
    }
    per_cpu_init::with_current(|cpu| {
        let t = cpu.current;
        if t.is_null() {
            return None;
        }
        // SAFETY: invariant: `cpu.current` is the TCB this CPU runs, live
        // and touched only by this CPU while it runs, and `with_current`'s
        // IF=0 keeps it current; established by `thread_init::switch_now`.
        let tcb = unsafe { &mut *t };
        if fpu::switch_away(cpu.fp_owner, cpu.cpu_id, t as usize, tcb.fp_cpu)
            == fpu::SwitchAway::Save
        {
            fp_save(tcb);
        }
        // FXSAVE layout (Intel SDM Vol. 1, 10.5.1): FCW at 0, FSW at 2,
        // MXCSR at 24.
        let b = &tcb.fpu.bytes;
        let fcw = u32::from(u16::from_le_bytes([b[0], b[1]]));
        let fsw = u32::from(u16::from_le_bytes([b[2], b[3]]));
        let mxcsr = u32::from_le_bytes([b[24], b[25], b[26], b[27]]);
        Some((fsw, fcw, mxcsr))
    })
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

/// A spawned or forked thread's first return to ring 3: the ordinary
/// syscall exit over the user frame its creator wrote at the top of its
/// kernel stack (`thread_init::spawn_user`). Runs `cli`, checks IF in
/// debug builds, loads the user data selectors, sets `KERNEL_GS_BASE` =
/// `PerCpu` before the selector loads (which zero the GS base), then
/// `GS_BASE` = `PerCpu`, `KERNEL_GS_BASE` = 0 (the user GS base) and
/// `FS_BASE` = `fs_base`, and jumps to `vibeos_syscall_return`.
///
/// # Safety
/// The running thread is a user thread whose kernel stack holds, at its
/// top, the user frame `thread_init::spawn_user` wrote, with a loaded CR3
/// that maps its user RIP and RSP.
pub unsafe fn first_return(fs_base: u64) -> ! {
    #[cfg(feature = "kernel_tests")]
    testing::on_first_return();
    // SAFETY: invariant I25: `kernel_rsp0` is the top of this thread's
    // kernel stack, whose top 168 bytes are its user frame, and the pad
    // word below it is where `vibeos_syscall_return` expects RSP; IF=0
    // from the `cli` to `sysretq` or `iretq`; established by
    // `thread_init::spawn_user` and `syscall_init::set_rsp0_for`.
    unsafe {
        core::arch::asm!(
            "cli",
            ".if {if_check}",
            "pushfq",
            "test qword ptr [rsp], 0x200",
            "lea rsp, [rsp + 8]",
            "jnz vibeos_enter_if_set",
            ".endif",
            "mov rdi, qword ptr gs:[0]",
            "mov r8, qword ptr gs:[{ksp}]",
            // KERNEL_GS_BASE = PerCpu first: `mov gs` below zeroes GS_BASE,
            // and an NMI in between swaps by the sign of GS_BASE.
            "mov ecx, {kernel_gs_base}",
            "mov eax, edi",
            "mov rdx, rdi",
            "shr rdx, 32",
            "wrmsr",
            "mov eax, {user_ss}",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            // kernel_tests: hold the window after the GS load open (ROADMAP
            // §10.2, F021); the call keeps the live RDI, RSI and R8, and
            // RSP 16-byte aligned (four pushes).
            #[cfg(feature = "kernel_tests")]
            "push rdi; push rsi; push r8; push r8; call {stall}; pop r8; pop r8; pop rsi; pop rdi",
            "mov ecx, {gs_base}",
            "mov eax, edi",
            "mov rdx, rdi",
            "shr rdx, 32",
            "wrmsr",
            "mov ecx, {kernel_gs_base}",
            "xor eax, eax",
            "xor edx, edx",
            "wrmsr",
            "mov ecx, {fs_base}",
            "mov eax, esi",
            "mov rdx, rsi",
            "shr rdx, 32",
            "wrmsr",
            "lea rsp, [r8 - {frame_pad}]",
            "jmp vibeos_syscall_return",
            if_check = const cfg!(debug_assertions) as u8,
            ksp = const KSP,
            kernel_gs_base = const IA32_KERNEL_GS_BASE,
            gs_base = const IA32_GS_BASE,
            fs_base = const IA32_FS_BASE,
            user_ss = const USER_DS_RPL as u32,
            frame_pad = const size_of::<UserFrame>() + PAD,
            #[cfg(feature = "kernel_tests")]
            stall = sym testing::fork_wait_stall_point,
            in("rsi") fs_base,
            options(noreturn),
        );
    }
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
pub extern "C" fn vibeos_syscall_stub(frame: *mut UserFrame) -> i64 {
    bump_counter();
    // SAFETY: invariant I25; the frame is the one
    // syscall_init::vibeos_syscall_entry built at the top of this thread's
    // kernel stack, which only this thread's syscall path refers to.
    let frame = unsafe { &mut *frame };
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
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

    use core::mem::size_of;
    use vibeos::paging::USER_END;

    use vibeos::syscall::UserFrame;

    use crate::time_init;

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

    /// The next `first_return` returns to a non-canonical RIP.
    pub(crate) fn arm_noncanonical_entry() {
        ARMED_ENTRY.store(true, Ordering::Release);
    }

    /// Processes the syscall exit's non-canonical path has killed.
    pub(crate) fn bad_rip_kills() -> u64 {
        BAD_RIP_KILLS.load(Ordering::Relaxed)
    }

    pub(super) fn on_exit(f: &mut UserFrame) {
        let t = crate::per_cpu_init::current_thread();
        // SAFETY: invariant: a non-null current thread is this CPU's live
        // TCB while it runs; established by `per_cpu_init::set_current_thread`.
        if t.is_null() || unsafe { (*t).pid } == 0 {
            return;
        }
        let armed = f.orig_rax.wrapping_add(1);
        if ARMED_NR
            .compare_exchange(armed, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            f.rip = USER_END;
        }
    }

    /// Top of `first_return`: an armed entry gets a non-canonical RIP in
    /// the frame it returns over.
    pub(super) fn on_first_return() {
        if !ARMED_ENTRY.swap(false, Ordering::AcqRel) {
            return;
        }
        let _g = crate::x86::InterruptGuard::enter();
        let top = crate::per_cpu_init::current().kernel_rsp0;
        let at = top - size_of::<UserFrame>() as u64;
        // SAFETY: invariant I25: `kernel_rsp0` is the top of this thread's
        // kernel stack, whose top 168 bytes are the user frame its creator
        // wrote and nothing else refers to; established by
        // `thread_init::spawn_user`.
        unsafe { (*(at as *mut UserFrame)).rip = USER_END };
    }

    /// First ring-3 entries [`fork_wait_stall_point`] still holds.
    static FORK_WAIT_STALLS: AtomicU32 = AtomicU32::new(0);

    /// How long [`fork_wait_stall_point`] holds one entry: two ticks of
    /// the 1 kHz timer.
    const FORK_WAIT_STALL_NS: u64 = 2_000_000;

    /// Hold the next `n` first ring-3 entries at [`fork_wait_stall_point`];
    /// 0 disarms.
    pub(crate) fn arm_fork_wait_stall(n: u32) {
        FORK_WAIT_STALLS.store(n, Ordering::Release);
    }

    /// In a first ring-3 entry, once GS is loaded for ring 3: while armed,
    /// spin for [`FORK_WAIT_STALL_NS`], so an interrupt the entry takes
    /// there lands inside that window (ROADMAP §10.2, F021). It reads no
    /// `gs:` operand, since GS already names the user's base.
    pub(crate) extern "C" fn fork_wait_stall_point() {
        if FORK_WAIT_STALLS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_err()
        {
            return;
        }
        let t0 = time_init::now_ns();
        while time_init::now_ns().saturating_sub(t0) < FORK_WAIT_STALL_NS {
            core::hint::spin_loop();
        }
    }
}
