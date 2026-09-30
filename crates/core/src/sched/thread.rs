//! Thread ids, TCB, and context-switch frame. ROADMAP §3.1–§3.2.
//!
//! Portable half: types and the synthetic frame layout. The switch itself
//! lives in each port's hardware half (`arch::x86_64::switch` on x86_64).
//! The TCB table, KVA mapping, and `spawn` live in the binary crate.

use core::mem::{offset_of, size_of};

use crate::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use crate::paging::{PAGE_SIZE_4K, VirtAddr};
use crate::pmm::Frames;
use crate::time::Instant;

/// Global TCB table size. UP today; phase 4 still addresses by id.
pub use crate::limits::MAX_THREADS;

/// x86 reserved-1 bit. `prepare_thread` seeds this and leaves IF clear.
pub const RFLAGS_RESERVED1: u64 = 0x2;
/// `RFLAGS.IF`. `schedule` ORs this on resume when `irq_nest == 0`.
pub const RFLAGS_IF: u64 = 1 << 9;

/// A thread's id, from `proc::pid::PidAlloc`, which pids share (a
/// process's pid is its first thread's tid). Never a table index: the
/// thread table finds a TCB through a lookup (`proc::pid::IdIndex`). 0 is
/// the bootstrap thread, which the allocator never hands out.
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

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Ready,
    Running,
    Sleeping {
        deadline: Instant,
    },
    /// Blocked on a wait queue. `wq` is the `WaitQueue` address, or 0.
    /// `deadline` is the one its timeout entry holds, `FAR_DEADLINE` when
    /// the wait has none, which the blocked-thread sweep checks
    /// (`sched::find_overdue`).
    Blocked {
        wq: usize,
        deadline: Instant,
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

pub use crate::limits::MAX_STACK_PAGES;

/// A guarded kernel stack: `pages` mapped pages above one unmapped guard
/// page at `guard` (default 4×4 KiB, DESIGN §4.5), and the order-0
/// [`Frames`] of each mapped page, lowest first. A move-only handle with
/// private fields: only `kva_init::alloc_guarded_stack` builds one
/// (through [`GuardedStack::from_raw_parts`]) and only `kva_init::free_stack`
/// takes it apart, so the stack and its frames are freed once, by their
/// owner. It lives in this crate so `Tcb` can own it.
pub struct GuardedStack {
    guard: VirtAddr,
    pages: usize,
    frames: [Option<Frames>; MAX_STACK_PAGES],
}

impl GuardedStack {
    /// # Safety
    /// `[guard, guard + (pages + 1) * 4 KiB)` came from
    /// `Kva::alloc_guarded(pages)`, the guard page is not mapped, upper
    /// page `i` maps `frames[i]` for every `i < pages`, the other slots are
    /// `None`, and no other `GuardedStack` names the range.
    pub unsafe fn from_raw_parts(
        guard: VirtAddr,
        pages: usize,
        frames: [Option<Frames>; MAX_STACK_PAGES],
    ) -> Self {
        Self {
            guard,
            pages,
            frames,
        }
    }

    /// Give the range and its frames back to the allocator that built it.
    ///
    /// # Safety
    /// The caller unmaps the range and shoots it down on every CPU before
    /// any of its frames or its VA is reused.
    pub unsafe fn into_raw_parts(self) -> (VirtAddr, usize, [Option<Frames>; MAX_STACK_PAGES]) {
        (self.guard, self.pages, self.frames)
    }

    /// The unmapped guard page.
    pub fn guard(&self) -> VirtAddr {
        self.guard
    }

    /// The lowest mapped byte.
    pub fn base(&self) -> VirtAddr {
        VirtAddr(self.guard.as_u64() + PAGE_SIZE_4K)
    }

    /// One past the highest mapped byte: the initial RSP.
    pub fn top(&self) -> VirtAddr {
        VirtAddr(self.guard.as_u64() + (self.pages as u64 + 1) * PAGE_SIZE_4K)
    }

    /// Mapped pages, the guard page not counted.
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// The frame each mapped page holds, lowest page first.
    pub fn frames(&self) -> &[Option<Frames>] {
        &self.frames[..self.pages.min(MAX_STACK_PAGES)]
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

/// A TCB's on-CPU flag (DESIGN §2.8 rule 2): set while a CPU runs the
/// thread or is still switching off it. The atomic is private, so each
/// store and load carries its order here and no caller names one.
#[repr(transparent)]
pub struct OnCpu(AtomicBool);

impl OnCpu {
    /// A clear flag. `const` outside `cfg(loom)`, whose atomics have no
    /// `const fn new` (C-ATOMICS).
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// A clear flag (loom's atomics have no `const fn new`).
    #[cfg(loom)]
    pub fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// A set flag, for a TCB built for the thread already running on this
    /// CPU (the bootstrap and AP idle threads). `const` outside `cfg(loom)`.
    #[cfg(not(loom))]
    pub const fn new_set() -> Self {
        Self(AtomicBool::new(true))
    }

    /// A set flag (loom's atomics have no `const fn new`).
    #[cfg(loom)]
    pub fn new_set() -> Self {
        Self(AtomicBool::new(true))
    }

    /// The incoming side of a switch marks the thread on this CPU.
    #[inline(always)]
    pub fn set(&self) {
        // Relaxed (P10-S08): the scheduler lock, held across the switch's
        // bookkeeping, orders this store with every reader that could see
        // the thread Dead; nothing is published through it.
        self.0.store(true, Ordering::Relaxed);
    }

    /// The switch tail's last access to the TCB it switched off, after
    /// `switch_context` has saved every piece of the thread's CPU state.
    #[inline(always)]
    pub fn clear(&self) {
        // Release: pairs with the Acquire load in `is_clear`, so a CPU that
        // sees the flag clear sees every save into the TCB before it.
        self.0.store(false, Ordering::Release);
    }

    /// True once no CPU runs or is switching off the thread: `spawn_inner`'s
    /// Dead-slot reuse check and the reaper.
    #[inline(always)]
    pub fn is_clear(&self) -> bool {
        // Acquire: pairs with the Release store in `clear`.
        !self.0.load(Ordering::Acquire)
    }
}

#[cfg(not(loom))]
impl Default for OnCpu {
    fn default() -> Self {
        Self::new()
    }
}

/// Global TCB.
#[repr(C, align(16))]
pub struct Tcb {
    pub id: ThreadId,
    pub name: &'static str,
    pub state: ThreadState,
    /// Set while a CPU runs this thread or is still switching off it: the
    /// incoming side of `thread_init::switch_now` sets it ([`OnCpu::set`]),
    /// and `thread_init::finish_switch` on that CPU clears it with Release
    /// ([`OnCpu::clear`]) as its last access to the TCB, after
    /// `switch_context` has saved every piece of DESIGN §7.5's per-thread
    /// state. `thread_init::spawn_inner` reuses a Dead slot only after an
    /// Acquire load finds it clear ([`OnCpu::is_clear`]), and ROADMAP
    /// §13.8's core dump and §17.4's `ptrace` requests wait on it too. A
    /// running bootstrap or AP idle TCB starts set.
    pub on_cpu: OnCpu,
    pub stack: Option<GuardedStack>,
    pub context: CpuContext,
    pub entry: fn(),
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
    /// The CPU that last loaded or held this thread's FP state, `None`
    /// until its first return to user mode and after any write to `fpu`
    /// (DESIGN §7.5, the FP binding; `vibeos::fpu`).
    pub fp_cpu: Option<u32>,
    /// Syscalls this thread has entered. Its own entry bumps it
    /// (`syscall_init::bump_counter`); other threads read it for the
    /// per-process sum (`thread_init::sum_syscalls`), so it is atomic. A
    /// statistic: its Relaxed accesses order nothing.
    pub syscall_count: AtomicU64,
    /// 0 = kernel thread. Process pid otherwise.
    pub pid: u32,
    /// Nonzero while this thread is a no-reclaim thread, which releases no
    /// counted object in place (DESIGN §2.11 rule 6): the threaded-IRQ
    /// bottom half for life, and a workqueue worker while it runs a
    /// softirq-equivalent item (`sync_init::no_reclaim`). Atomic, because
    /// `Sched::get_mut` builds `&mut Tcb` for threads running elsewhere.
    pub no_reclaim: AtomicU32,
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

/// One slot of the kernel's TCB table (`thread_init`'s `Sched.slots`): null,
/// or a pointer to a `Tcb`. The core tool reads the table through
/// `SYMBOL(vibeos_tcbs)` (docs/VMCOREINFO.md).
pub type TcbSlot = Option<crate::kalloc::TryBox<Tcb>>;

// The layout the core tool reads (docs/VMCOREINFO.md, "Types the core tool
// reads"), in the kernel and in every hostlib build (ROADMAP §10.7).
// Outside `cfg(loom)`, whose atomics differ in size. `Tcb.stack` holds
// `MAX_STACK_PAGES` frame tokens (`pmm::Frames`), each 16 bytes with debug
// assertions, which record its allocation site, and 8 without, so the
// fields after it move with the profile.
#[cfg(not(loom))]
const _: () = {
    use core::mem::align_of;
    const DEBUG: bool = cfg!(debug_assertions);
    /// `s`'s tag: a `#[repr(u32)]` enum starts with its `u32` tag.
    const fn tag(s: &ThreadState) -> u32 {
        // SAFETY: `ThreadState` is `#[repr(u32)]` (its definition, above),
        // so every variant begins with an initialized `u32` tag at offset 0
        // and `s` is aligned for it; established here.
        unsafe { *core::ptr::from_ref(s).cast::<u32>() }
    }
    assert!(tag(&ThreadState::Ready) == 0);
    assert!(tag(&ThreadState::Running) == 1);
    assert!(
        tag(&ThreadState::Sleeping {
            deadline: Instant { ns: 0 }
        }) == 2
    );
    assert!(
        tag(&ThreadState::Blocked {
            wq: 0,
            deadline: Instant { ns: 0 }
        }) == 3
    );
    assert!(tag(&ThreadState::Dead) == 4);
    assert!(size_of::<ThreadState>() == 24);
    assert!(align_of::<ThreadState>() == 8);
    assert!(size_of::<Tcb>() == if DEBUG { 1264 } else { 1008 });
    assert!(align_of::<Tcb>() == 16);
    assert!(offset_of!(Tcb, id) == 0);
    assert!(offset_of!(Tcb, state) == 24);
    assert!(offset_of!(Tcb, context) == if DEBUG { 592 } else { 336 });
    assert!(offset_of!(Tcb, cpu) == if DEBUG { 680 } else { 424 });
    assert!(offset_of!(Tcb, pid) == if DEBUG { 1248 } else { 992 });
    assert!(size_of::<CpuContext>() == 72);
    assert!(size_of::<TcbSlot>() == size_of::<usize>());
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

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn on_cpu_clear_is_seen() {
        use crate::atomic::AtomicU64;
        use std::sync::Arc;
        assert!(OnCpu::new().is_clear());
        assert!(!OnCpu::new_set().is_clear());
        // A payload the switch-out writes before its clear: a thread that
        // sees the flag clear must see it (the Release/Acquire pair).
        let flag = Arc::new(OnCpu::new());
        let saved = Arc::new(AtomicU64::new(0));
        flag.set();
        assert!(!flag.is_clear());
        let switcher = {
            let (flag, saved) = (Arc::clone(&flag), Arc::clone(&saved));
            std::thread::spawn(move || {
                saved.store(0xC0FFEE, Ordering::Relaxed);
                flag.clear();
            })
        };
        while !flag.is_clear() {
            crate::atomic::spin_loop();
        }
        assert_eq!(saved.load(Ordering::Relaxed), 0xC0FFEE);
        switcher.join().unwrap();
    }

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
        assert_eq!(
            ThreadState::Blocked {
                wq: 0,
                deadline: Instant { ns: 1 }
            }
            .name(),
            "blocked"
        );
        assert_eq!(ThreadState::Dead.name(), "dead");
        assert_eq!(WaitOutcome::Woken.name(), "woken");
        assert_eq!(WaitOutcome::Timeout.name(), "timeout");
        assert_eq!(CpuAffinity::Any.name(), "any");
        assert_eq!(CpuAffinity::Pinned(1).name(), "pinned");
        assert_eq!(ThreadId::BOOTSTRAP.raw(), 0);
        assert!(ThreadId::NONE.is_none());
        assert_eq!(size_of::<ThreadId>(), 4);
    }
}
