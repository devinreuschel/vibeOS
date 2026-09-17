//! Thread ids, TCB, and context-switch frame. ROADMAP §3.1–§3.2.
//!
//! Portable half: types, synthetic frame layout, `switch_context` asm.
//! The TCB table, KVA mapping, and `spawn` live in the binary crate.

use core::mem::{offset_of, size_of};

use crate::time::Instant;

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

/// Placement for phase 4 per-CPU queues. Slice A stores it; Slice B/4 use it.
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
    /// Wait-queue cookie filled by Slice C. 0 means "blocked, queue unknown".
    Blocked,
    Dead,
}

impl ThreadState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Sleeping { .. } => "sleeping",
            Self::Blocked => "blocked",
            Self::Dead => "dead",
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

/// Global TCB. `next`/`prev` are the run-queue links Slice B fills.
#[repr(C)]
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
    pub switches: u64,
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
            // Bit 1 is reserved-1 on x86 flags.
            rflags: 0x2,
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
};

/// SysV: `rsp % 16 == 8` on function entry. `stack_top` must be 16-aligned.
/// IF is off. Caller may store a dummy 0 at `rsp` so a stray `ret` faults.
pub fn prepare_thread(ctx: &mut CpuContext, stack_top: u64, entry: u64) {
    assert!(stack_top % 16 == 0, "thread stack top must be 16-aligned");
    *ctx = CpuContext::empty();
    ctx.rip = entry;
    ctx.rsp = stack_top - 8;
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
}

unsafe extern "C" {
    fn vibeos_switch_context(old: *mut CpuContext, new: *const CpuContext);
}

/// Save callee-saved GPRs, rflags, rsp, return address; restore `new`.
///
/// Kernel builds `cli` after the save so a timer cannot observe mixed
/// GPRs. Host tests skip `cli` (ring 3). No FPU/SSE.
///
/// # Safety
/// `old` and `new` must be valid. `new.rsp` must point at a live stack.
/// Caller is not using the red zone below either rsp.
pub unsafe fn switch_context(old: *mut CpuContext, new: *const CpuContext) {
    unsafe { vibeos_switch_context(old, new) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};
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
        assert_eq!(ctx.rflags, 0x2);
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
        assert_eq!(ThreadState::Blocked.name(), "blocked");
        assert_eq!(ThreadState::Dead.name(), "dead");
        assert_eq!(CpuAffinity::Any.name(), "any");
        assert_eq!(CpuAffinity::Pinned(1).name(), "pinned");
        assert_eq!(ThreadId::BOOTSTRAP.raw(), 0);
        assert!(ThreadId::NONE.is_none());
        assert_eq!(size_of::<ThreadId>(), 4);
    }

    static FLAG: AtomicU64 = AtomicU64::new(0);
    static MAIN_PTR: std::sync::atomic::AtomicPtr<CpuContext> =
        std::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    static WORKER_PTR: std::sync::atomic::AtomicPtr<CpuContext> =
        std::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

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
