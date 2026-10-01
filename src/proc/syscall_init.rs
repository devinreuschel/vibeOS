//! STAR / LSTAR / FMASK / SCE, syscall entry asm, FPU, RSP0/CR3 on switch.
//! ROADMAP §9.1 / §9.3. Same entry as Slice A; stub is the dispatch table.

use core::arch::global_asm;
use core::mem::{offset_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use vibeos::arch::SyscallAbi;
use vibeos::arch::x86_64::trap::sysret_ok;
use vibeos::desc::{USER_CS_RPL, USER_DS_RPL, UserSegs, star_value};
use vibeos::fpu;
use vibeos::per_cpu::PerCpu;
use vibeos::syscall::UserFrame;
use vibeos::thread::{Fxsave, Tcb, ThreadId};
use vibeos::vectors;

use crate::arch::current::Arch;
use crate::arch::gdt::{self, CpuTables};
use crate::arch::idt::TrapFrame;
use crate::per_cpu_init;
use crate::x86::{
    self, EFER_SCE, FMASK_SYSCALL, IA32_EFER, IA32_FMASK, IA32_FS_BASE, IA32_GS_BASE,
    IA32_KERNEL_GS_BASE, IA32_LSTAR, IA32_STAR,
};

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
const ENOSYS_RET: i64 = -(vibeos::kerror::KError::NoSys.errno() as i64);

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

        // The user frame (vibeos::arch::x86_64::trap::UserFrame), top down:
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
        // The irqoff tracer (INVARIANTS.md §2.9 rule 2): the entry's IF=0
        // stretch, from here to the `sti`. Everything live is in the frame.
        .if {irqoff}
        mov edi, {site_entry}
        call {irqoff_off}
        call {irqoff_on}
        .endif
        // The body runs with IF=1 (DESIGN §2.9 rule 3). Not before the
        // pushes above: another thread's `syscall` on this CPU overwrites
        // gs:[user_rsp].
        sti

        lea rdi, [rsp + {pad}]
        // The frame holds the user rbp; a null rbp ends the kernel's
        // frame-pointer chain here, so a backtrace from the body stops at
        // this entry instead of following the user's (DESIGN §2.5 step 4).
        xor ebp, ebp
        call vibeos_syscall_stub
        // IF is off from here to sysretq or iretq (AGENTS.md rule 2).
        cli
        mov [rsp + {f_rax}], rax
        .if {irqoff}
        mov edi, {site_exit}
        call {irqoff_off}
        .endif

    // The return to ring 3 over the user frame at RSP + 8: IF=0, kernel
    // GS. `first_return` enters here too.
    .global vibeos_syscall_return
    vibeos_syscall_return:
        // Exit work (DESIGN §5.10 rule 11): the last check for a pending
        // kill or stop, with IF=0; it turns IF on only to do found work.
        mov edi, {exit_syscall}
        lea rsi, [rsp + {pad}]
        call vibeos_exit_work
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
        // The tracer's stretch ends after the last exit work; the few
        // instructions from here to `sysretq` or `iretq` are unmeasured.
        .if {irqoff}
        call {irqoff_on}
        .endif
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
    irqoff = const cfg!(feature = "irqoff") as u8,
    irqoff_off = sym crate::sched::irqoff::vibeos_irqoff_off_site,
    irqoff_on = sym crate::sched::irqoff::vibeos_irqoff_on,
    site_entry = const crate::sched::irqoff::Site::SYSCALL_ENTRY.bits(),
    site_exit = const crate::sched::irqoff::Site::SYSCALL_EXIT.bits(),
    user_cs = const USER_CS_RPL as u64,
    user_ss = const USER_DS_RPL as u64,
    exit_syscall = const EXIT_SYSCALL,
);

/// [`exit_work`]'s `kind` for the syscall exit (and a new thread's first
/// return); a vector exit passes its vector, below this.
pub const EXIT_SYSCALL: u64 = 0x100;

/// The process layer's exit-work hooks (DESIGN §1.2): whether the current
/// process has a kill or stop to act on, read with IF=0, and the work
/// itself, run with IF=1. `proc_init::init` sets both before the first
/// ring-3 entry; unset, no exit has work.
static EXIT_PENDING: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());
static EXIT_WORK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the exit-work hooks.
pub fn set_exit_work_hooks(pending: fn() -> bool, work: fn(&mut UserFrame)) {
    // Release: pairs with the Acquire loads in `exit_work`.
    EXIT_WORK.store(work as *mut (), Ordering::Release);
    EXIT_PENDING.store(pending as *mut (), Ordering::Release);
}

/// The syscall exit's call into [`exit_work`].
///
/// # Safety
/// `frame` is the user frame the exit is returning over.
#[unsafe(no_mangle)]
unsafe extern "C" fn vibeos_exit_work(kind: u64, frame: *mut UserFrame) {
    // SAFETY: invariant I25: `frame` is this thread's user frame at the top
    // of its kernel stack, which nothing else refers to at the exit;
    // established by `syscall_init::vibeos_syscall_entry` or
    // `thread_init::spawn_user`.
    exit_work(kind, unsafe { &mut *frame });
}

/// Exit work on a return to ring 3 (DESIGN §5.10 rule 11): entered with
/// IF=0 after the exit's `cli`; while the current process has a kill or a
/// stop pending, turn IF on, act on it (a kill does not return), turn IF
/// off, and check again. Returns with IF=0 once a check finds none. `kind`
/// is [`EXIT_SYSCALL`] or the vector whose exit this is.
pub fn exit_work(kind: u64, frame: &mut UserFrame) {
    #[cfg(feature = "kernel_tests")]
    crate::proc::ktest::exit_seen(frame);
    // Acquire: pairs with the Release stores in `set_exit_work_hooks`.
    let pending = EXIT_PENDING.load(Ordering::Acquire);
    let work = EXIT_WORK.load(Ordering::Acquire);
    if !pending.is_null() && !work.is_null() {
        // SAFETY: invariant: non-null hooks hold a `fn() -> bool` and a
        // `fn(&mut UserFrame)`; established by
        // `syscall_init::set_exit_work_hooks`, their only stores.
        let (pending, work) = unsafe {
            (
                core::mem::transmute::<*mut (), fn() -> bool>(pending),
                core::mem::transmute::<*mut (), fn(&mut UserFrame)>(work),
            )
        };
        loop {
            debug_assert!(!x86::interrupts_enabled(), "exit work check with IF on");
            if !pending() {
                break;
            }
            #[cfg(feature = "kernel_tests")]
            crate::proc::ktest::exit_work_found(kind);
            x86::sti();
            work(frame);
            x86::cli();
        }
    }
    #[cfg(feature = "kernel_tests")]
    if kind == EXIT_SYSCALL {
        crate::proc::ktest::exit_check_hook(frame);
    }
    #[cfg(not(feature = "kernel_tests"))]
    let _ = kind;
}

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
    crate::arch::idt::user_fault(&TrapFrame::for_user(vectors::GP, 0, f));
    #[allow(
        clippy::panic,
        reason = "invariant: a thread returns to ring 3 only as a process's thread, and `proc_init::init` installs the fault hook before the first ring-3 entry, so `user_fault` kills it and does not return"
    )]
    {
        panic!(
            "syscall: non-canonical return RIP {:#x} with no process",
            f.rip
        );
    }
}

/// The exit's choice between `sysretq` and `iretq` (Linux's rule,
/// `arch::x86_64::trap::sysret_ok`).
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
    let star = star_value();
    // SAFETY: STAR, LSTAR, FMASK and EFER are architectural MSRs that
    // every x86_64 CPU has; STAR names the GDT's selectors and LSTAR the
    // entry stub, and EFER keeps its other bits. The GDT is loaded (this
    // fn's contract, `syscall_init::init_cpu`).
    unsafe {
        x86::wrmsr(IA32_STAR, star);
        x86::wrmsr(IA32_LSTAR, entry);
        x86::wrmsr(IA32_FMASK, FMASK_SYSCALL);
        let efer = x86::rdmsr(IA32_EFER);
        x86::wrmsr(IA32_EFER, efer | EFER_SCE);
    }
}

/// BSP: attach the GDT TSS and the dedicated RSP0 stack.
///
/// # Safety
/// GDT loaded, `GS_BASE` is the BSP `PerCpu`.
pub unsafe fn init_bsp() {
    // SAFETY: the GDT is loaded and `GS_BASE` is the BSP's `PerCpu` (this
    // fn's contract, `syscall_init::init_bsp`).
    unsafe { init_cpu() };
    crate::arch::idt::set_user_return_hook(user_return);
    crate::thread_init::set_switch_hooks(on_switch);
    per_cpu_init::with_current(|cpu| {
        cpu.tables = gdt::bsp_tables().cast();
        let top = gdt::bsp_rsp0_top();
        cpu.fallback_rsp0 = top;
        cpu.kernel_rsp0 = top;
        cpu.remote
            .as_cr3
            .store(crate::paging_init::kernel_cr3(), Ordering::Release);
    });
    // `vibeos.strace=1` on the kernel command line (BOOT.md §3.2).
    if crate::boot::cmdline().flag("vibeos.strace") {
        set_trace(true);
    }
    crate::log_init::apply_boot_level();
}

/// AP: `tables` is this CPU's GDT/TSS. Call after `install_gs`.
///
/// # Safety
/// `tables` is this CPU's loaded `CpuTables`, live while it runs; `rsp0`
/// is its kernel stack top.
pub unsafe fn init_ap(tables: *const CpuTables, rsp0: u64) {
    // SAFETY: the AP loaded its GDT and `GS_BASE` before this call (this
    // fn's contract, `syscall_init::init_ap`).
    unsafe { init_cpu() };
    per_cpu_init::with_current(|cpu| {
        cpu.tables = tables.cast();
        cpu.fallback_rsp0 = rsp0;
        cpu.kernel_rsp0 = rsp0;
        cpu.remote
            .as_cr3
            .store(crate::paging_init::kernel_cr3(), Ordering::Release);
    });
}

// The kernel's only FP and SIMD instructions (`scripts/check_kernel_fp.py`
// allows these two routines and no other).

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
    // a 16-byte aligned FXSAVE image with MXCSR's reserved bits clear
    // (`Fxsave::INITIAL` or a save of this hardware); established by
    // `arch::cpu::init_control_regs`, `vibeos::thread::Fxsave::INITIAL` and
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
    per_cpu_init::with_current(|cpu| {
        let t = crate::arch::current_tcb();
        if t.is_null() {
            return;
        }
        // SAFETY: invariant: `current_tcb` is the TCB this CPU runs, live
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

/// `arch::idt`'s user-return hook, run by `idt::exit_to_user` after its
/// `cli`: the exit work, except on an NMI's exit, which keeps IF=0 (DESIGN
/// §5.10 rule 3), then the FP binding check.
fn user_return(frame: &mut TrapFrame) {
    if frame.vector != u64::from(vectors::NMI) {
        exit_work(frame.vector, frame.user_mut());
    }
    vibeos_fp_user_return();
}

/// The running thread's x87 status word, x87 control word, and MXCSR,
/// read under the FP binding's read rule (DESIGN §7.5, C-FPBIND): inside
/// an `InterruptGuard`, this CPU's registers are saved into `Tcb.fpu`
/// first when they hold its state. `None` with no current thread.
pub fn current_fp_words() -> Option<(u32, u32, u32)> {
    per_cpu_init::with_current(|cpu| {
        let t = save_current_fp(cpu);
        if t.is_null() {
            return None;
        }
        // SAFETY: invariant: a non-null `current_tcb` is the TCB this CPU
        // runs, live and touched only by this CPU while it runs, and
        // `with_current`'s IF=0 keeps it current; established by
        // `thread_init::switch_now`.
        let tcb = unsafe { &*t };
        // FXSAVE layout (Intel SDM Vol. 1, 10.5.1): FCW at 0, FSW at 2,
        // MXCSR at 24.
        let b = &tcb.fpu.bytes;
        let fcw = u32::from(u16::from_le_bytes([b[0], b[1]]));
        let fsw = u32::from(u16::from_le_bytes([b[2], b[3]]));
        let mxcsr = u32::from_le_bytes([b[24], b[25], b[26], b[27]]);
        Some((fsw, fcw, mxcsr))
    })
}

/// The running thread's TCB, with its FP state saved into `Tcb.fpu` first
/// when this CPU's registers hold it (the binding's read rule, DESIGN §7.5).
/// `cpu` is this CPU's `PerCpu`, held with IF=0. Null with no current
/// thread.
fn save_current_fp(cpu: &PerCpu) -> *mut Tcb {
    let t = crate::arch::current_tcb();
    if t.is_null() {
        return t;
    }
    // SAFETY: invariant: `current_tcb` is the TCB this CPU runs, live and
    // touched only by this CPU while it runs, and the caller's IF=0 keeps it
    // current; established by `thread_init::switch_now`.
    let tcb = unsafe { &mut *t };
    if fpu::switch_away(cpu.fp_owner, cpu.cpu_id, t as usize, tcb.fp_cpu) == fpu::SwitchAway::Save {
        fp_save(tcb);
    }
    t
}

/// `fork`'s FP state (DESIGN §7.5, F069): in one IF=0 stretch, save the
/// caller's live registers under the binding, copy its `Tcb.fpu` into
/// `child`, a thread not yet made ready, and empty the child's `fp_cpu`,
/// so its first return to ring 3 loads the copy.
pub fn fork_fp(child: ThreadId) {
    let c = crate::thread_init::tcb_ptr(child);
    if c.is_null() {
        return;
    }
    per_cpu_init::with_current(|cpu| {
        let p = save_current_fp(cpu);
        if p.is_null() {
            return;
        }
        // SAFETY: invariant I9: `p` is the TCB this CPU runs and `c` a
        // live TCB that stays allocated, two distinct threads; the child
        // is not yet runnable, so only its creator, here, touches it, and
        // `with_current`'s IF=0 keeps `p` current; established by
        // `thread_init::spawn_user` and `thread_init::switch_now`.
        unsafe {
            (*c).fpu = (*p).fpu;
            crate::thread_init::fp_invalidate(&mut *c);
        }
    });
}

/// `execve`'s FP state (DESIGN §7.5, F069, F129): in one IF=0 stretch,
/// write [`Fxsave::INITIAL`] into the running thread's `Tcb.fpu` and empty
/// its `fp_cpu`, so a switch before the return to ring 3 saves nothing over
/// it and that return loads it.
pub fn exec_fp() {
    let _irq = x86::InterruptGuard::enter();
    let t = crate::arch::current_tcb();
    if t.is_null() {
        return;
    }
    // SAFETY: invariant: `current_tcb` is the TCB this CPU runs, live and
    // touched only by this CPU while it runs, and the guard's IF=0 keeps it
    // current; established by `thread_init::switch_now`.
    let tcb = unsafe { &mut *t };
    tcb.fpu = Fxsave::INITIAL;
    crate::thread_init::fp_invalidate(tcb);
}

/// Update TSS.RSP0 + `kernel_rsp0` for `tcb`. Every context switch.
///
/// # Safety
/// `cpu` is this CPU's own `PerCpu`, held with IF=0 (`on_switch` holds it
/// so), and `cpu.tables` is null or the `CpuTables` this CPU loaded, which
/// `syscall_init::init_bsp` and `syscall_init::init_ap` establish.
pub unsafe fn set_rsp0_for(cpu: &mut PerCpu, tcb: &Tcb) {
    let top = match &tcb.stack {
        Some(s) => s.top().as_u64(),
        None => cpu.fallback_rsp0,
    };
    cpu.kernel_rsp0 = top;
    let tables = cpu.tables.cast::<CpuTables>();
    if !tables.is_null() {
        // SAFETY: `CpuTables::set_rsp0`'s contract: `tables` is the tables
        // this CPU loaded and IF=0, both by this fn's `# Safety`,
        // established by `syscall_init::init_bsp` and
        // `syscall_init::init_ap` and by the caller's IF=0 stretch.
        unsafe { (*tables).set_rsp0(top) };
    }
}

/// Load `tcb`'s CR3 if it differs. Skip when the next thread shares AS.
///
/// # Safety
/// `cpu` is this CPU's own `PerCpu`, held with IF=0 as `on_switch` holds
/// it. `tcb.as_cr3` is 0 or a root that invariant I44 keeps alive while a
/// TCB names it (the core's free, `addr_space_init::SpaceCore`).
pub unsafe fn switch_cr3_for(cpu: &mut PerCpu, tcb: &Tcb) -> bool {
    let want = if tcb.as_cr3 == 0 {
        crate::paging_init::kernel_cr3()
    } else {
        tcb.as_cr3
    };
    if cpu.remote.as_cr3.load(Ordering::Relaxed) == want || want == 0 {
        return true;
    }
    // SAFETY: invariant I44: `want` is the kernel root or a TCB's root,
    // which stays allocated while that TCB names it, and every root shares
    // the kernel half this code and stack run in; established by
    // `addr_space_init::SpaceCore`'s drop.
    unsafe { x86::write_cr3(want) };
    cpu.remote.as_cr3.store(want, Ordering::Release);
    false
}

/// The switch away from `old`: save its FP state if this CPU's registers
/// hold it (the FP binding, DESIGN §7.5). Loads nothing: the next return
/// to ring 3 does.
///
/// # Safety
/// `cpu` is this CPU's own `PerCpu`, held with IF=0, and `old` is null or
/// the live TCB this CPU is switching off (invariant I9), which no other
/// CPU writes until the switch tail clears its `on_cpu`; the caller,
/// `thread_init::switch_now` through [`on_switch`], establishes both.
pub unsafe fn switch_fpu(cpu: &mut PerCpu, old: *mut Tcb) {
    if old.is_null() {
        return;
    }
    // SAFETY: invariant I9: `old` is the TCB this CPU is switching off, live
    // until the switch tail clears its `on_cpu`, with IF=0; established
    // by this fn's `# Safety`, which `thread_init::switch_now` meets.
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
/// `switch_context`.
///
/// # Safety
/// `old` and `new` are null or live TCBs (invariant I9), `new` the one
/// that runs next on this CPU, IF=0, and `cpu` is this CPU's `PerCpu`
/// with `cpu.tables` as `init_bsp` or `init_ap` set it;
/// `thread_init::switch_now` establishes each, inside
/// `with_current_switch`.
pub unsafe fn on_switch(cpu: &mut PerCpu, old: *mut Tcb, new: *mut Tcb) {
    // SAFETY: `switch_fpu`'s contract, which this fn's `# Safety` covers;
    // established by `thread_init::switch_now`.
    unsafe { switch_fpu(cpu, old) };
    if !new.is_null() {
        // SAFETY: invariant I9: `new` is the live TCB this CPU is switching
        // to, and `cpu` this CPU's `PerCpu` held with IF=0, as
        // `set_rsp0_for` and `switch_cr3_for` require, with `cpu.tables` as
        // `init_bsp` or `init_ap` set it; established by this fn's
        // `# Safety`, which `thread_init::switch_now` meets.
        unsafe {
            set_rsp0_for(cpu, &*new);
            switch_cr3_for(cpu, &*new);
        }
    }
}

/// A spawned or forked thread's first return to ring 3: the ordinary
/// syscall exit over the user frame its creator wrote at the top of its
/// kernel stack (`thread_init::spawn_user`). Runs `cli`, checks IF in
/// debug builds, loads the thread's `Tcb.user_segs` into DS, ES, FS and GS
/// (DESIGN §5.1), sets `KERNEL_GS_BASE` = `PerCpu` before those loads
/// (which zero the GS base), then
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
    // The asm's own `cli` finds IF already off; this one tells the irqoff
    // tracer where the return's stretch began.
    crate::arch::current::irq_disable();
    let t = crate::arch::current_tcb();
    let segs = if t.is_null() {
        UserSegs::NULL
    } else {
        // SAFETY: invariant: `current_tcb` is the TCB this CPU runs, live
        // and written only by this thread or under its parent's fork before
        // `make_ready`; established by `thread_init::switch_now`.
        unsafe { (*t).user_segs }
    };
    let segs = u64::from(segs.ds)
        | u64::from(segs.es) << 16
        | u64::from(segs.fs) << 32
        | u64::from(segs.gs) << 48;
    // The kernel_tests fork-wait stall spins inside the asm below with
    // IF=0 and may read no `gs:` there, so its stretch is marked here.
    #[cfg(feature = "kernel_tests")]
    if testing::fork_wait_stall_armed() {
        crate::sched::irqoff::deliberate_open("fork-wait stall");
    }
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
            // The thread's DS, ES, FS and GS, 16 bits each from R9's low end
            // (`Tcb.user_segs`: null, or the parent's after `fork`).
            "mov rax, r9",
            "mov ds, ax",
            "shr rax, 16",
            "mov es, ax",
            "shr rax, 16",
            "mov fs, ax",
            "shr rax, 16",
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
            frame_pad = const size_of::<UserFrame>() + PAD,
            #[cfg(feature = "kernel_tests")]
            stall = sym testing::fork_wait_stall_point,
            in("rsi") fs_base,
            in("r9") segs,
            options(noreturn),
        );
    }
}

// --- Slice B: dispatch, early fd1 ---

static TRACE: AtomicBool = AtomicBool::new(false);

pub fn set_trace(on: bool) {
    TRACE.store(on, Ordering::Release);
}

pub fn trace_enabled() -> bool {
    TRACE.load(Ordering::Acquire)
}

fn bump_counter() {
    let t = per_cpu_init::current_thread();
    if !t.is_null() {
        // SAFETY: invariant I9: a non-null current thread is this CPU's
        // live TCB, which stays in `SCHED`; established by
        // `per_cpu_init::set_current_thread`.
        let n = unsafe { &(*t).syscall_count };
        // Relaxed: the count is a statistic and pairs with nothing.
        n.fetch_add(1, Ordering::Relaxed);
    }
}

/// The syscall handler: the process layer's `syscall`, which its `init`
/// sets before the first ring-3 entry (DESIGN §1.2). Unset, every syscall
/// returns `-ENOSYS`.
static HANDLER: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the syscall handler.
pub fn set_syscall_handler(f: fn(&mut UserFrame) -> i64) {
    // Release: pairs with the Acquire load in `vibeos_syscall_stub`.
    HANDLER.store(f as *mut (), Ordering::Release);
}

/// The syscall entry's Rust half: the one place the user frame's raw
/// pointer becomes `&mut UserFrame`, which the handler
/// (`proc_init::syscall`) takes.
///
/// # Safety
/// `frame` is the user frame `vibeos_syscall_entry` built at the top of
/// this thread's kernel stack (invariant I25), which nothing else refers
/// to while the syscall runs; that entry asm is the only caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vibeos_syscall_stub(frame: *mut UserFrame) -> i64 {
    bump_counter();
    // SAFETY: invariant I25; the frame is the one
    // `syscall_init::vibeos_syscall_entry` built at the top of this thread's
    // kernel stack, which only this thread's syscall path refers to
    // (this fn's `# Safety`).
    let frame = unsafe { &mut *frame };
    #[cfg(feature = "kernel_tests")]
    crate::log::ktest::syscall_walk_probe(frame.rbp);
    let nr = <Arch as SyscallAbi>::nr(frame);
    vibeos::trace!(SyscallEnter, nr, <Arch as SyscallAbi>::arg(frame, 0));
    // Acquire: pairs with the Release store in `set_syscall_handler`.
    let p = HANDLER.load(Ordering::Acquire);
    let r = if p.is_null() {
        ENOSYS_RET
    } else {
        // SAFETY: invariant: a non-null `HANDLER` holds a
        // `fn(&mut UserFrame) -> i64`; established by
        // `syscall_init::set_syscall_handler`, its only store.
        let f = unsafe { core::mem::transmute::<*mut (), fn(&mut UserFrame) -> i64>(p) };
        f(frame)
    };
    #[cfg(feature = "kernel_tests")]
    testing::on_exit(frame);
    vibeos::trace!(SyscallExit, nr, r as u64);
    r
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

    /// Whether [`fork_wait_stall_point`] still holds an entry.
    pub(super) fn fork_wait_stall_armed() -> bool {
        FORK_WAIT_STALLS.load(Ordering::Acquire) != 0
    }

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
