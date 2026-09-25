//! Thread ids, TCB, and context-switch frame. ROADMAP §3.1–§3.2.
//!
//! Portable half: types, synthetic frame layout, `switch_context` asm.
//! The TCB table, KVA mapping, and `spawn` live in the binary crate.

use core::mem::{offset_of, size_of};

use crate::paging::PAGE_SIZE_4K;
use crate::time::Instant;

/// Global TCB table size. UP today; phase 4 still addresses by id.
pub use crate::limits::MAX_THREADS;

/// x86 reserved-1 bit. `prepare_thread` seeds this and leaves IF clear.
pub const RFLAGS_RESERVED1: u64 = 0x2;
/// `RFLAGS.IF`. `schedule` ORs this on resume when `irq_nest == 0`.
pub const RFLAGS_IF: u64 = 1 << 9;

/// Slot index in the global TCB table. 0 is the bootstrap thread.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(pub u32);

impl ThreadId {
    pub const BOOTSTRAP: Self = Self(0);
    /// Empty idle / ready-head slot. Not a table index.
    pub const NONE: Self = Self(u32::MAX);

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn is_none(self) -> bool {
        self.0 == u32::MAX
    }
}

/// Placement for per-CPU queues. `Any` round-robins; `Pinned` stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuAffinity {
    Any,
    Pinned(u32),
}

impl CpuAffinity {
    pub fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Pinned(_) => "pinned",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Ready,
    Running,
    Sleeping {
        deadline: Instant,
    },
    /// Blocked on a wait queue. `wq` is the `WaitQueue` address, or 0.
    Blocked {
        wq: usize,
    },
    Dead,
}

impl ThreadState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Sleeping { .. } => "sleeping",
            Self::Blocked { .. } => "blocked",
            Self::Dead => "dead",
        }
    }
}

/// Why `wait` resumed. Timeout path writes this under SCHED before ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Woken,
    Timeout,
}

impl WaitOutcome {
    pub fn name(self) -> &'static str {
        match self {
            Self::Woken => "woken",
            Self::Timeout => "timeout",
        }
    }
}

/// Guarded kernel stack identity (default 4×4 KiB + unmapped guard).
/// Mapping is `kva_init`'s; this is only the VA so `Tcb` can live here.
#[derive(Clone, Copy, Debug)]
pub struct KernelStack {
    pub guard: u64,
    pub pages: usize,
}

impl KernelStack {
    pub fn top(self) -> u64 {
        self.guard + (self.pages as u64 + 1) * PAGE_SIZE_4K
    }
}

/// FXSAVE area. 16-byte aligned. Initialized from a template at FPU bring-up.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct Fxsave {
    pub bytes: [u8; 512],
}

impl Fxsave {
    pub const fn empty() -> Self {
        Self { bytes: [0; 512] }
    }
}

/// Global TCB. `next`/`prev` are the run-queue links Slice B fills.
#[repr(C, align(16))]
pub struct Tcb {
    pub id: ThreadId,
    pub name: &'static str,
    pub state: ThreadState,
    pub stack: Option<KernelStack>,
    pub context: CpuContext,
    pub entry: fn(),
    /// Intrusive ready-list link. Slice B; phase 4 is per-CPU.
    pub next: Option<ThreadId>,
    pub prev: Option<ThreadId>,
    pub affinity: CpuAffinity,
    pub cpu: u32,
    /// `InterruptGuard` depth frozen while this thread is off-CPU.
    /// `switch_to` / `schedule` swap it with `PerCpu.irq_nest` so a
    /// one-way exit does not leak the dying stack's nest onto the CPU.
    pub irq_nest: u32,
    pub switches: u64,
    /// TSC cycles accounted while this thread was current.
    pub run_tsc: u64,
    /// Last `wait` result. Valid after `schedule` returns from a wait.
    pub wait_outcome: WaitOutcome,
    /// User CR3. 0 means the shared kernel PML4.
    pub as_cr3: u64,
    pub fpu: Fxsave,
    /// Syscall counter. Aggregated per-process in Slice C.
    pub syscall_count: u64,
    /// 0 = kernel thread. Process pid otherwise.
    pub pid: u32,
}

/// Callee-saved GPRs, rflags, rsp, return address. No XMM: soft-float.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CpuContext {
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub rip: u64,
}

impl CpuContext {
    pub const RBX: usize = 0;
    pub const RBP: usize = 8;
    pub const R12: usize = 16;
    pub const R13: usize = 24;
    pub const R14: usize = 32;
    pub const R15: usize = 40;
    pub const RFLAGS: usize = 48;
    pub const RSP: usize = 56;
    pub const RIP: usize = 64;

    pub const fn empty() -> Self {
        Self {
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rflags: RFLAGS_RESERVED1,
            rsp: 0,
            rip: 0,
        }
    }
}

const _: () = {
    assert!(offset_of!(CpuContext, rbx) == CpuContext::RBX);
    assert!(offset_of!(CpuContext, rbp) == CpuContext::RBP);
    assert!(offset_of!(CpuContext, r12) == CpuContext::R12);
    assert!(offset_of!(CpuContext, r13) == CpuContext::R13);
    assert!(offset_of!(CpuContext, r14) == CpuContext::R14);
    assert!(offset_of!(CpuContext, r15) == CpuContext::R15);
    assert!(offset_of!(CpuContext, rflags) == CpuContext::RFLAGS);
    assert!(offset_of!(CpuContext, rsp) == CpuContext::RSP);
    assert!(offset_of!(CpuContext, rip) == CpuContext::RIP);
    assert!(size_of::<CpuContext>() == 72);
    assert!(size_of::<ThreadId>() == 4);
    assert!(offset_of!(Tcb, fpu) % 16 == 0);
};

/// SysV: `rsp % 16 == 8` on function entry. `stack_top` must be 16-aligned.
/// IF is off (`rflags = 0x2`). `schedule` applies [`apply_if_on_resume`]
/// so a first-run or timer-preempted thread is not stuck tick-deaf.
pub fn prepare_thread(ctx: &mut CpuContext, stack_top: u64, entry: u64) {
    assert!(
        stack_top.is_multiple_of(16),
        "thread stack top must be 16-aligned"
    );
    *ctx = CpuContext::empty();
    ctx.rip = entry;
    ctx.rsp = stack_top - 8;
}

/// IF-on-resume policy (Kernel Design, Slice A nit).
///
/// `switch_context` saves CPU rflags. Inside an ISR or `InterruptGuard`
/// that is IF=0, so a raw restore would leave the thread deaf until some
/// later `sti`. Incoming threads with `irq_nest == 0` run with IF set.
/// Nested guards keep IF clear until that stack's guard drops (or `iret`
/// restores IF from the interrupt frame). A switch to `irq_nest == 0`
/// therefore enables IF on the incoming thread even if the outgoing
/// stack still has an open ISR / `InterruptGuard` — expected; the
/// guard lives on the preempted stack.
pub fn apply_if_on_resume(rflags: &mut u64, irq_nest: u32) {
    if irq_nest == 0 {
        *rflags |= RFLAGS_IF;
    } else {
        *rflags &= !RFLAGS_IF;
    }
}

#[cfg(target_arch = "x86_64")]
mod switch_asm {
    use super::CpuContext;
    use core::arch::global_asm;

    // Host unit tests cannot `cli` (ring 3 #GP). Kernel builds cli after
    // saving rflags so the GPR shuffle is not preempted.
    #[cfg(test)]
    global_asm!(
        r#"
        .pushsection .text
        .global vibeos_switch_context
        .type vibeos_switch_context, @function
        vibeos_switch_context:
            mov rax, [rsp]
            mov [rdi + {rip}], rax
            lea rax, [rsp + 8]
            mov [rdi + {rsp}], rax
            mov [rdi + {rbx}], rbx
            mov [rdi + {rbp}], rbp
            mov [rdi + {r12}], r12
            mov [rdi + {r13}], r13
            mov [rdi + {r14}], r14
            mov [rdi + {r15}], r15
            pushfq
            pop rax
            mov [rdi + {rflags}], rax
            mov rbx, [rsi + {rbx}]
            mov rbp, [rsi + {rbp}]
            mov r12, [rsi + {r12}]
            mov r13, [rsi + {r13}]
            mov r14, [rsi + {r14}]
            mov r15, [rsi + {r15}]
            mov rsp, [rsi + {rsp}]
            mov rax, [rsi + {rflags}]
            push rax
            popfq
            jmp qword ptr [rsi + {rip}]
        .popsection
        "#,
        rbx = const CpuContext::RBX,
        rbp = const CpuContext::RBP,
        r12 = const CpuContext::R12,
        r13 = const CpuContext::R13,
        r14 = const CpuContext::R14,
        r15 = const CpuContext::R15,
        rflags = const CpuContext::RFLAGS,
        rsp = const CpuContext::RSP,
        rip = const CpuContext::RIP,
    );

    #[cfg(not(test))]
    global_asm!(
        r#"
        .pushsection .text
        .global vibeos_switch_context
        .type vibeos_switch_context, @function
        vibeos_switch_context:
            mov rax, [rsp]
            mov [rdi + {rip}], rax
            lea rax, [rsp + 8]
            mov [rdi + {rsp}], rax
            mov [rdi + {rbx}], rbx
            mov [rdi + {rbp}], rbp
            mov [rdi + {r12}], r12
            mov [rdi + {r13}], r13
            mov [rdi + {r14}], r14
            mov [rdi + {r15}], r15
            pushfq
            pop rax
            mov [rdi + {rflags}], rax
            cli
            mov rbx, [rsi + {rbx}]
            mov rbp, [rsi + {rbp}]
            mov r12, [rsi + {r12}]
            mov r13, [rsi + {r13}]
            mov r14, [rsi + {r14}]
            mov r15, [rsi + {r15}]
            mov rsp, [rsi + {rsp}]
            mov rax, [rsi + {rflags}]
            test rax, {rflags_if}
            jz 2f
            and rax, {rflags_no_if}
            push rax
            popfq
            sti
            jmp qword ptr [rsi + {rip}]
        2:
            push rax
            popfq
            jmp qword ptr [rsi + {rip}]
        .popsection
        "#,
        rbx = const CpuContext::RBX,
        rbp = const CpuContext::RBP,
        r12 = const CpuContext::R12,
        r13 = const CpuContext::R13,
        r14 = const CpuContext::R14,
        r15 = const CpuContext::R15,
        rflags = const CpuContext::RFLAGS,
        rflags_if = const super::RFLAGS_IF,
        rflags_no_if = const !super::RFLAGS_IF,
        rsp = const CpuContext::RSP,
        rip = const CpuContext::RIP,
    );
}

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn vibeos_switch_context(old: *mut CpuContext, new: *const CpuContext);
}

/// Save callee-saved GPRs, rflags, rsp, return address; restore `new`.
///
/// Kernel builds `cli` after the save so a timer cannot observe mixed
/// GPRs. Incoming IF is applied with delayed `sti` immediately before
/// `jmp`, not `popfq` with IF set: a tick in that window preempts a
/// first-run thread, `schedule_preempt` overwrites the synthetic
/// trampoline frame, and `iret` then `jmp`s into `schedule_inner` on
/// `stack_top-8`. Host tests skip `cli` (ring 3). No FPU/SSE.
///
/// # Safety
/// `old` and `new` must be valid. `new.rsp` must point at a live stack.
/// Caller is not using the red zone below either rsp.
#[cfg(target_arch = "x86_64")]
pub unsafe fn switch_context(old: *mut CpuContext, new: *const CpuContext) {
    unsafe { vibeos_switch_context(old, new) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};
    #[cfg(target_arch = "x86_64")]
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn context_layout() {
        assert_eq!(offset_of!(CpuContext, rbx), CpuContext::RBX);
        assert_eq!(offset_of!(CpuContext, rbp), CpuContext::RBP);
        assert_eq!(offset_of!(CpuContext, r12), CpuContext::R12);
        assert_eq!(offset_of!(CpuContext, r13), CpuContext::R13);
        assert_eq!(offset_of!(CpuContext, r14), CpuContext::R14);
        assert_eq!(offset_of!(CpuContext, r15), CpuContext::R15);
        assert_eq!(offset_of!(CpuContext, rflags), CpuContext::RFLAGS);
        assert_eq!(offset_of!(CpuContext, rsp), CpuContext::RSP);
        assert_eq!(offset_of!(CpuContext, rip), CpuContext::RIP);
        assert_eq!(size_of::<CpuContext>(), 72);
    }

    #[test]
    fn prepare_thread_sysv_align() {
        let mut ctx = CpuContext::empty();
        let top = 0xFFFF_D000_0001_0000u64;
        prepare_thread(&mut ctx, top, 0x1111);
        assert_eq!(ctx.rsp % 16, 8);
        assert_eq!(ctx.rip, 0x1111);
        assert_eq!(ctx.rflags, RFLAGS_RESERVED1);
        assert_eq!(ctx.rflags & RFLAGS_IF, 0);
    }

    #[test]
    fn if_on_resume_policy() {
        let mut r = RFLAGS_RESERVED1;
        apply_if_on_resume(&mut r, 0);
        assert_eq!(r & RFLAGS_IF, RFLAGS_IF);
        apply_if_on_resume(&mut r, 1);
        assert_eq!(r & RFLAGS_IF, 0);
        apply_if_on_resume(&mut r, 0);
        assert_eq!(r & RFLAGS_IF, RFLAGS_IF);
    }

    #[test]
    fn thread_state_names() {
        assert_eq!(ThreadState::Ready.name(), "ready");
        assert_eq!(ThreadState::Running.name(), "running");
        assert_eq!(
            ThreadState::Sleeping {
                deadline: Instant { ns: 1 }
            }
            .name(),
            "sleeping"
        );
        assert_eq!(ThreadState::Blocked { wq: 0 }.name(), "blocked");
        assert_eq!(ThreadState::Dead.name(), "dead");
        assert_eq!(WaitOutcome::Woken.name(), "woken");
        assert_eq!(WaitOutcome::Timeout.name(), "timeout");
        assert_eq!(CpuAffinity::Any.name(), "any");
        assert_eq!(CpuAffinity::Pinned(1).name(), "pinned");
        assert_eq!(ThreadId::BOOTSTRAP.raw(), 0);
        assert!(ThreadId::NONE.is_none());
        assert_eq!(size_of::<ThreadId>(), 4);
    }

    #[cfg(target_arch = "x86_64")]
    static FLAG: AtomicU64 = AtomicU64::new(0);
    #[cfg(target_arch = "x86_64")]
    static MAIN_PTR: std::sync::atomic::AtomicPtr<CpuContext> =
        std::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    #[cfg(target_arch = "x86_64")]
    static WORKER_PTR: std::sync::atomic::AtomicPtr<CpuContext> =
        std::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

    #[cfg(target_arch = "x86_64")]
    extern "C" fn worker_entry() {
        FLAG.store(0xC0FFEE, Ordering::SeqCst);
        unsafe {
            switch_context(
                WORKER_PTR.load(Ordering::SeqCst),
                MAIN_PTR.load(Ordering::SeqCst),
            );
        }
        panic!("worker resumed");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn switch_context_roundtrip() {
        FLAG.store(0, Ordering::SeqCst);
        let mut buf = vec![0u8; 16 * 1024 + 16];
        let base = buf.as_mut_ptr() as usize;
        let top = ((base + buf.len()) & !15) as u64;
        let mut main_ctx = CpuContext::empty();
        let mut worker_ctx = CpuContext::empty();
        let main_p = &raw mut main_ctx;
        let worker_p = &raw mut worker_ctx;
        MAIN_PTR.store(main_p, Ordering::SeqCst);
        WORKER_PTR.store(worker_p, Ordering::SeqCst);
        unsafe {
            prepare_thread(&mut *worker_p, top, worker_entry as *const () as u64);
            ((*worker_p).rsp as *mut u64).write_volatile(0);
            switch_context(main_p, worker_p);
        }
        assert_eq!(FLAG.load(Ordering::SeqCst), 0xC0FFEE);
        assert!(main_ctx.rip != 0);
        assert!(main_ctx.rsp != 0);
    }
}
