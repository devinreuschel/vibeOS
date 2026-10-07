//! aarch64 syscall / switch hooks: `svc` entry, `eret` exit, TTBR0
//! (ROADMAP §11.6).

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::arch::current::UserFrame;
use vibeos::arch::SyscallAbi;
use vibeos::fpu;
use vibeos::kerror::KError;
use vibeos::paging::USER_MAP_END;
use vibeos::per_cpu::PerCpu;
use vibeos::proc::uaccess::USER_TAG_MASK;
use vibeos::thread::{Fxsave, Tcb, ThreadId};

use crate::arch::current::{Arch, InterruptGuard};
use crate::per_cpu_init;

pub const EXIT_SYSCALL: u64 = 0x100;

static HANDLER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static EXIT_PENDING: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static EXIT_WORK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn set_exit_work_hooks(pending: fn() -> bool, work: fn(&mut UserFrame)) {
    // Release: pairs with the Acquire loads in `exit_work`.
    EXIT_WORK.store(work as *mut (), Ordering::Release);
    // Release: pairs with the Acquire load in `exit_work`.
    EXIT_PENDING.store(pending as *mut (), Ordering::Release);
}

/// Exit work on a return to EL0 (DESIGN §5.10 rule 11): entered with
/// DAIF all set; while work is pending, unmask, act, mask, and check
/// again.
pub fn exit_work(kind: u64, frame: &mut UserFrame) {
    // Acquire: pairs with the Release stores in `set_exit_work_hooks`.
    let pending = EXIT_PENDING.load(Ordering::Acquire);
    // Acquire: pairs with the Release store in `set_exit_work_hooks`.
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
            if !pending() {
                break;
            }
            crate::arch::aarch64::cpu::daif_clear_all();
            work(frame);
            crate::arch::aarch64::cpu::daif_set_all();
        }
    }
    let _ = kind;
}

/// # Safety
/// Unused: secondaries load the full VBAR in `vectors::load`.
#[expect(
    dead_code,
    reason = "x86 MSR bring-up counterpart; aarch64 writes none"
)]
pub unsafe fn init_cpu() {}

/// # Safety
/// TPIDR is the BSP `PerCpu`.
pub unsafe fn init_bsp() {
    crate::thread_init::set_switch_hooks(on_switch);
    let empty = crate::paging_init::empty_user_root();
    per_cpu_init::with_current(|cpu| {
        // Release: pairs with the Acquire load in `addr_space_init::root_holder`.
        cpu.remote
            .as_cr3
            .store(empty, core::sync::atomic::Ordering::Release);
    });
    if empty != 0 {
        // SAFETY: `empty` is the permanent ASID-0 TTBR0 root
        // `paging_init::install` published; established there.
        unsafe { crate::arch::aarch64::cpu::write_ttbr0(empty) };
    }
    crate::log_init::apply_boot_level();
}

/// # Safety
/// Secondaries: VBAR is live; this writes the empty TTBR0 root.
pub unsafe fn init_ap(_tables: *const crate::arch::gdt::CpuTables, _rsp0: u64) {
    let empty = crate::paging_init::empty_user_root();
    if empty != 0 {
        // SAFETY: `empty` is the permanent ASID-0 TTBR0 root
        // `paging_init::install` published; established there.
        unsafe { crate::arch::aarch64::cpu::write_ttbr0(empty) };
    }
}

/// Save V0–V31, FPCR, and FPSR into `tcb.fpu` (ROADMAP §11.6, F069).
#[inline(never)]
pub fn fp_save(tcb: &mut Tcb) {
    let p = core::ptr::from_mut(&mut tcb.fpu).cast::<u8>();
    // SAFETY: invariant: `tcb.fpu` is a 16-byte aligned 528-byte `Fxsave`
    // inside a live TCB, and `CPACR_EL1.FPEN` is already 0b11 from
    // `cpu::apply_computed_sysregs`; established by `vibeos::thread::Tcb`
    // and `arch::aarch64::cpu::apply_computed_sysregs`.
    unsafe {
        // Soft-float kernel: encode NEON/FPCR here so rustc's assembler
        // does not need +fp. `x0` is the image; encodings from
        // `aarch64-linux-gnu-as` for `stp qN, qN+1, [x0, #32*(N/2)]`.
        core::arch::asm!(
            ".inst 0xad000400",
            ".inst 0xad010c02",
            ".inst 0xad021404",
            ".inst 0xad031c06",
            ".inst 0xad042408",
            ".inst 0xad052c0a",
            ".inst 0xad06340c",
            ".inst 0xad073c0e",
            ".inst 0xad084410",
            ".inst 0xad094c12",
            ".inst 0xad0a5414",
            ".inst 0xad0b5c16",
            ".inst 0xad0c6418",
            ".inst 0xad0d6c1a",
            ".inst 0xad0e741c",
            ".inst 0xad0f7c1e",
            ".inst 0xd53b4401",
            "str w1, [x0, #512]",
            ".inst 0xd53b4421",
            "str w1, [x0, #516]",
            in("x0") p,
            out("x1") _,
            options(nostack),
        );
    }
}

/// Load `tcb.fpu` into V0–V31, FPCR, and FPSR. Only from
/// `vibeos_fp_user_return`, with DAIF set.
#[inline(never)]
fn fp_load(tcb: &Tcb) {
    let p = core::ptr::from_ref(&tcb.fpu).cast::<u8>();
    // SAFETY: invariant: `tcb.fpu` is a 16-byte aligned image from
    // `Fxsave::ZERO` or `fp_save`, and `CPACR_EL1.FPEN` is 0b11;
    // established by `vibeos::thread::Fxsave::ZERO` and
    // `syscall_init::fp_save`.
    unsafe {
        core::arch::asm!(
            ".inst 0xad400400",
            ".inst 0xad410c02",
            ".inst 0xad421404",
            ".inst 0xad431c06",
            ".inst 0xad442408",
            ".inst 0xad452c0a",
            ".inst 0xad46340c",
            ".inst 0xad473c0e",
            ".inst 0xad484410",
            ".inst 0xad494c12",
            ".inst 0xad4a5414",
            ".inst 0xad4b5c16",
            ".inst 0xad4c6418",
            ".inst 0xad4d6c1a",
            ".inst 0xad4e741c",
            ".inst 0xad4f7c1e",
            "ldr w1, [x0, #512]",
            ".inst 0xd51b4401",
            "ldr w1, [x0, #516]",
            ".inst 0xd51b4421",
            in("x0") p,
            out("x1") _,
            options(nostack),
        );
    }
}

/// The FP binding check on every return to EL0 (DESIGN §7.5).
#[unsafe(no_mangle)]
pub extern "C" fn vibeos_fp_user_return() {
    per_cpu_init::with_current(|cpu| {
        let t = crate::arch::current_tcb();
        if t.is_null() {
            return;
        }
        // SAFETY: invariant: `current_tcb` is the TCB this CPU runs, live
        // and touched only by this CPU while it runs, and DAIF is set;
        // established by `thread_init::switch_now`.
        let tcb = unsafe { &mut *t };
        let (me, addr) = (cpu.cpu_id, t as usize);
        if fpu::user_return(cpu.fp_owner, me, addr, tcb.fp_cpu) == fpu::UserReturn::Load {
            fp_load(tcb);
            fpu::bind(&mut cpu.fp_owner, me, addr, &mut tcb.fp_cpu);
        }
    });
}

#[expect(dead_code, reason = "x86 #MF/#XM refine; aarch64 uses ESR")]
pub fn current_fp_words() -> Option<(u32, u32, u32)> {
    None
}

fn save_current_fp(cpu: &PerCpu) -> *mut Tcb {
    let t = crate::arch::current_tcb();
    if t.is_null() {
        return t;
    }
    // SAFETY: invariant: `current_tcb` is the TCB this CPU runs; the
    // caller's DAIF-set stretch keeps it current; established by
    // `thread_init::switch_now`.
    let tcb = unsafe { &mut *t };
    if fpu::switch_away(cpu.fp_owner, cpu.cpu_id, t as usize, tcb.fp_cpu) == fpu::SwitchAway::Save {
        fp_save(tcb);
    }
    t
}

/// `fork`'s FP state (DESIGN §7.5, F069).
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
        // SAFETY: invariant I9: `p` is this CPU's TCB and `c` a live TCB
        // not yet runnable; established by `thread_init::spawn_user`.
        unsafe {
            (*c).fpu = (*p).fpu;
            crate::thread_init::fp_invalidate(&mut *c);
        }
    });
}

/// `execve`'s FP state: V0–V31, FPCR, and FPSR zero (ROADMAP §11.6).
pub fn exec_fp() {
    let _irq = InterruptGuard::enter();
    let t = crate::arch::current_tcb();
    if t.is_null() {
        return;
    }
    // SAFETY: invariant: `current_tcb` is this CPU's live TCB; the
    // guard keeps it current; established by `thread_init::switch_now`.
    let tcb = unsafe { &mut *t };
    tcb.fpu = Fxsave::ZERO;
    crate::thread_init::fp_invalidate(tcb);
}

/// # Safety
/// `cpu` is this CPU's `PerCpu`.
#[expect(dead_code, reason = "aarch64 has no TSS RSP0")]
pub unsafe fn set_rsp0_for(_cpu: &mut PerCpu, _tcb: &Tcb) {}

/// Load `tcb`'s TTBR0 if it differs. A kernel thread (`as_cr3` 0) gets
/// the empty user root.
///
/// # Safety
/// `cpu` is this CPU's `PerCpu`, held with IRQs masked as `on_switch`
/// holds it. `tcb.as_cr3` is 0 or a user root invariant I44 keeps alive.
pub unsafe fn switch_cr3_for(cpu: &mut PerCpu, tcb: &Tcb) -> bool {
    let want = if tcb.as_cr3 == 0 {
        crate::paging_init::empty_user_root()
    } else {
        tcb.as_cr3
    };
    // Relaxed: only this CPU stores its `as_cr3`; pairs with nothing.
    if cpu.remote.as_cr3.load(Ordering::Relaxed) == want || want == 0 {
        return true;
    }
    // SAFETY: invariant I44: `want` is the empty user root or a TCB's
    // TTBR0, which stays allocated while that TCB names it; established
    // by `addr_space_init::SpaceCore`'s drop and `paging_init::install`.
    unsafe { crate::arch::aarch64::cpu::write_ttbr0(want) };
    crate::arch::aarch64::cpu::tlbi_all();
    // Release: pairs with the Acquire load in `addr_space_init::root_holder`.
    cpu.remote.as_cr3.store(want, Ordering::Release);
    false
}

/// The switch away from `old`: save its FP state if this CPU's registers
/// hold it (the FP binding, DESIGN §7.5).
///
/// # Safety
/// `cpu` is this CPU's `PerCpu`, held with IRQs masked, and `old` is null
/// or the live TCB this CPU is switching off (invariant I9);
/// `thread_init::switch_now` through [`on_switch`] establishes both.
pub unsafe fn switch_fpu(cpu: &mut PerCpu, old: *mut Tcb) {
    if old.is_null() {
        return;
    }
    // SAFETY: invariant I9: `old` is the TCB this CPU is switching off;
    // established here.
    let old = unsafe { &mut *old };
    if fpu::switch_away(
        cpu.fp_owner,
        cpu.cpu_id,
        core::ptr::from_mut(old) as usize,
        old.fp_cpu,
    ) == fpu::SwitchAway::Save
    {
        fp_save(old);
    }
}

/// # Safety
/// As `thread_init::switch_now`.
pub unsafe fn on_switch(cpu: &mut PerCpu, old: *mut Tcb, new: *mut Tcb) {
    if !old.is_null() {
        // SAFETY: invariant I9: `old` is the TCB this CPU is switching
        // off; `TPIDR_EL0` is still the outgoing user TLS;
        // established by `thread_init::switch_now`.
        unsafe {
            if (*old).pid != 0 {
                (*old).tls_base = crate::arch::current::user_tls();
            }
        }
    }
    // SAFETY: `switch_fpu`'s contract, which this fn's `# Safety` covers;
    // established by `thread_init::switch_now`.
    unsafe { switch_fpu(cpu, old) };
    if !new.is_null() {
        // SAFETY: invariant I9: `new` is the live TCB this CPU is switching
        // to, and `cpu` this CPU's `PerCpu` held with IRQs masked;
        // established by `thread_init::switch_now`.
        unsafe {
            switch_cr3_for(cpu, &*new);
            if (*new).pid != 0 {
                crate::arch::current::set_user_tls((*new).tls_base);
            }
        }
    }
}

/// A spawned thread's first return to EL0 over the user frame at the
/// top of its kernel stack.
///
/// # Safety
/// The running thread is a user thread whose kernel stack holds, at its
/// top, the user frame `thread_init::spawn_user` wrote, with a loaded
/// TTBR0 that maps its user PC and SP.
pub unsafe fn first_return(fs_base: u64) -> ! {
    crate::arch::aarch64::cpu::daif_set_all();
    let t = crate::arch::current_tcb();
    if !t.is_null() {
        // SAFETY: invariant I9: `current_tcb` is this CPU's live TCB;
        // established by `thread_init::switch_now`.
        unsafe { (*t).tls_base = fs_base };
    }
    // SAFETY: `fs_base` is the image's thread pointer, a user address
    // the loader chose; established by `user_init::setup_tls` or 0.
    unsafe { crate::arch::current::set_user_tls(fs_base) };
    if t.is_null() {
        crate::arch::current::halt();
    }
    // SAFETY: invariant I9: `current_tcb` is this CPU's live TCB;
    // established by `thread_init::switch_now`.
    let top = unsafe { (*t).stack.as_ref().map(|s| s.top().as_u64()).unwrap_or(0) };
    if top < core::mem::size_of::<UserFrame>() as u64 {
        crate::arch::current::halt();
    }
    let frame = top - core::mem::size_of::<UserFrame>() as u64;
    // SAFETY: invariant I25: `[top - 288, top)` is this thread's user
    // frame; SP becomes that address and `vibeos_el0_return` restores
    // from it; established by `thread_init::spawn_user`.
    unsafe {
        core::arch::asm!(
            "mov sp, {frame}",
            "b vibeos_el0_return",
            frame = in(reg) frame,
            options(noreturn),
        );
    }
}

#[expect(dead_code, reason = "x86 syscall tracer; unused on this port")]
pub fn set_trace(_on: bool) {}

pub fn trace_enabled() -> bool {
    false
}

pub fn set_syscall_handler(f: fn(&mut UserFrame) -> i64) {
    // Release: pairs with the Acquire load in `enter`.
    HANDLER.store(f as *mut (), Ordering::Release);
}

fn bump_counter() {
    let t = per_cpu_init::current_thread();
    if t.is_null() {
        return;
    }
    // SAFETY: invariant I9: a non-null current thread is this CPU's
    // live TCB; established by `per_cpu_init::set_current_thread`.
    let n = unsafe { &(*t).syscall_count };
    // Relaxed: the count is a statistic and pairs with nothing.
    n.fetch_add(1, Ordering::Relaxed);
}

/// The `svc` body's Rust half: the process layer's `syscall`.
pub fn enter(frame: &mut UserFrame) {
    bump_counter();
    let nr = frame.x[8] as u32 as u64;
    frame.orig_x0 = frame.x[0];
    frame.syscallno = nr;
    vibeos::trace!(SyscallEnter, nr, frame.x[0]);
    // Acquire: pairs with the Release store in `set_syscall_handler`.
    let h = HANDLER.load(Ordering::Acquire);
    let ret = if h.is_null() {
        -(KError::NoSys.errno() as i64)
    } else {
        // SAFETY: invariant: a non-null `HANDLER` holds a
        // `fn(&mut UserFrame) -> i64`; established by
        // `syscall_init::set_syscall_handler`, its only store.
        let f = unsafe { core::mem::transmute::<*mut (), fn(&mut UserFrame) -> i64>(h) };
        f(frame)
    };
    Arch::set_ret(frame, ret as u64);
}

/// Whether `pc` may be written to `ELR_EL1` for a return to EL0.
pub fn elr_ok(pc: u64) -> bool {
    pc < USER_MAP_END && pc & USER_TAG_MASK == 0
}

#[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// The exit stub's `ldarb` reads this. One byte, naturally aligned.
    #[unsafe(no_mangle)]
    static VIBEOS_ERET_BP_ARMED: AtomicBool = AtomicBool::new(false);
    static ERET_BP_HITS: AtomicU64 = AtomicU64::new(0);
    static SAVED_MDSCR: AtomicU64 = AtomicU64::new(0);
    static PROBE: AtomicBool = AtomicBool::new(false);
    static EXIT_PC: AtomicU64 = AtomicU64::new(0);
    static EXIT_PSTATE: AtomicU64 = AtomicU64::new(0);
    static EXIT_READY: AtomicBool = AtomicBool::new(false);

    /// E, PMC = EL1+EL0, BAS = 0b1111. HMC when this CPU is at EL2.
    ///
    /// An A64 address-match breakpoint with BAS 0 never fires. QEMU
    /// treats that as no breakpoint. PMC 0b11 does not match EL2 unless
    /// HMC is set, and VHE runs the exit there.
    fn eret_bcr() -> u64 {
        let mut bcr = 1u64 | (0b11 << 1) | (0xF << 5);
        if crate::arch::aarch64::cpu::current_el() == 2 {
            bcr |= 1 << 13;
        }
        bcr
    }

    pub(crate) fn arm_eret_breakpoint() {
        // Release: pairs with the Acquire load in `eret_bp_hits`.
        ERET_BP_HITS.store(0, Ordering::Release);
        let eret = vibeos_el0_eret_addr();
        let bcr = eret_bcr();
        let saved: u64;
        // SAFETY: MDSCR/DBGBVR/DBGBCR are writable at the kernel EL; the
        // breakpoint is this test's and targets the exit `eret`;
        // established here.
        unsafe {
            core::arch::asm!(
                "mrs {saved}, mdscr_el1",
                // MDE|KDE is not one logical immediate.
                "orr {t}, {saved}, {mde}",
                "orr {t}, {t}, {kde}",
                "msr mdscr_el1, {t}",
                "isb",
                "msr dbgbvr0_el1, {eret}",
                "msr dbgbcr0_el1, {bcr}",
                "isb",
                saved = out(reg) saved,
                t = out(reg) _,
                eret = in(reg) eret,
                bcr = in(reg) bcr,
                mde = const 1u64 << 15,
                kde = const 1u64 << 13,
            );
        }
        // Release: pairs with the Acquire load in `disarm_eret_breakpoint`.
        SAVED_MDSCR.store(saved, Ordering::Release);
        // Release: pairs with the Acquire load in `eret_breakpoint_armed`
        // and the ldarb in `vibeos_el0_return`.
        VIBEOS_ERET_BP_ARMED.store(true, Ordering::Release);
    }

    pub(crate) fn disarm_eret_breakpoint() {
        // Release: pairs with the Acquire load in `eret_breakpoint_armed`
        // and the ldarb in `vibeos_el0_return`.
        VIBEOS_ERET_BP_ARMED.store(false, Ordering::Release);
        // Release: pairs with the AcqRel swap in `take_probe`.
        PROBE.store(false, Ordering::Release);
        // Acquire: pairs with the Release store in `arm_eret_breakpoint`.
        let saved = SAVED_MDSCR.load(Ordering::Acquire);
        // SAFETY: restore the MDSCR this test saved and clear its
        // breakpoint; established here by `arm_eret_breakpoint`.
        unsafe {
            core::arch::asm!(
                "msr dbgbcr0_el1, xzr",
                "msr mdscr_el1, {saved}",
                "isb",
                saved = in(reg) saved,
                options(nostack, preserves_flags),
            );
        }
    }

    /// Drop the breakpoint match. The watch flag stays set so a D-clear
    /// exit still skips the debug-build halt until the test disarms.
    pub(crate) fn clear_eret_bcr() {
        // SAFETY: DBGBCR0_EL1 is writable at the kernel EL; established here.
        unsafe {
            core::arch::asm!(
                "msr dbgbcr0_el1, xzr",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }

    pub(crate) fn eret_breakpoint_armed() -> bool {
        // Acquire: pairs with the Release store in `arm_eret_breakpoint`.
        VIBEOS_ERET_BP_ARMED.load(Ordering::Acquire)
    }

    /// User PC and PSTATE the next `eret` would return to. The debug
    /// exception on that `eret` overwrites ELR and SPSR, so the handler
    /// finishes the return from these.
    pub(crate) fn note_exit_target(pc: u64, pstate: u64) {
        // Relaxed: pairs with nothing; published by the Release store below.
        EXIT_PC.store(pc, Ordering::Relaxed);
        // Relaxed: pairs with nothing; published by the Release store below.
        EXIT_PSTATE.store(pstate, Ordering::Relaxed);
        // Release: pairs with the AcqRel swap in `take_exit_target`.
        EXIT_READY.store(true, Ordering::Release);
    }

    pub(crate) fn take_exit_target() -> Option<(u64, u64)> {
        // AcqRel: pairs with the Release store in `note_exit_target`.
        if !EXIT_READY.swap(false, Ordering::AcqRel) {
            return None;
        }
        // Relaxed: pairs with nothing; the AcqRel swap above publishes them.
        let pc = EXIT_PC.load(Ordering::Relaxed);
        // Relaxed: pairs with nothing; the AcqRel swap above publishes them.
        let pstate = EXIT_PSTATE.load(Ordering::Relaxed);
        Some((pc, pstate))
    }

    pub(crate) fn begin_probe() {
        // Release: pairs with the AcqRel swap in `take_probe`.
        PROBE.store(true, Ordering::Release);
    }

    pub(crate) fn take_probe() -> bool {
        // AcqRel: pairs with the Release store in `begin_probe`.
        PROBE.swap(false, Ordering::AcqRel)
    }

    /// Branch at `vibeos_el0_eret` once with PSTATE.D clear.
    ///
    /// The handler returns to the continuation in x30. A miss executes
    /// the `eret`, whose ELR is that same label, so either way we land
    /// on `2` and restore DAIF. `mrs PAN` already has the bit in [22].
    pub(crate) fn poke_eret_once() {
        begin_probe();
        let eret = vibeos_el0_eret_addr();
        // SAFETY: DAIF, ELR_EL1, and SPSR_EL1 are writable at the kernel
        // EL; the branch targets the exit `eret`, and a missed breakpoint
        // returns through the ELR written here; established here.
        unsafe {
            core::arch::asm!(
                "mrs x9, daif",
                "mrs x10, nzcv",
                "orr x10, x10, x9",
                "mrs x11, pan",
                "orr x10, x10, x11",
                "mrs x11, CurrentEL",
                "mrs x12, spsel",
                "orr x11, x11, x12",
                "orr x10, x10, x11",
                "msr daifset, #2",
                "msr daifclr, #8",
                "isb",
                "adr x30, 2f",
                "msr elr_el1, x30",
                "msr spsr_el1, x10",
                "br x13",
                "2:",
                "msr daif, x9",
                in("x13") eret,
                out("x9") _,
                out("x10") _,
                out("x11") _,
                out("x12") _,
                out("x30") _,
                options(nostack),
            );
        }
        // The handler consumes the flag on a hit. A miss leaves it set.
        let _missed = take_probe();
    }

    pub(crate) fn eret_bp_hits() -> u64 {
        // Acquire: pairs with the Release store in `note_eret_bp`.
        ERET_BP_HITS.load(Ordering::Acquire)
    }

    pub(crate) fn note_eret_bp() {
        // Release: pairs with the Acquire load in `eret_bp_hits`.
        ERET_BP_HITS.fetch_add(1, Ordering::Release);
    }

    static BAD_ELR: AtomicBool = AtomicBool::new(false);

    /// The next return to EL0 writes `ELR_EL1` = 2^48 (ROADMAP §11.6).
    pub(crate) fn arm_bad_elr() {
        // Release: pairs with the AcqRel swap in `take_bad_elr`.
        BAD_ELR.store(true, Ordering::Release);
    }

    pub(crate) fn take_bad_elr() -> bool {
        // AcqRel: pairs with the Release store in `arm_bad_elr`.
        BAD_ELR.swap(false, Ordering::AcqRel)
    }

    fn vibeos_el0_eret_addr() -> u64 {
        unsafe extern "C" {
            fn vibeos_el0_eret();
        }
        vibeos_el0_eret as *const () as u64
    }
}
