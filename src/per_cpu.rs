//! Per-CPU struct layout. DESIGN §7.5.
//!
//! `self_ptr` is at offset 0 so `gs:[0]` yields the struct address.
//! Hardware install (`GS_BASE`) lives in the binary crate.
//!
//! Heap array sized from the MADT CPU count (no `MAX_CPUS` static).
//! `runq` is this CPU's ready FIFO; remotes use `wake_inbox` + IPI.
//!
//! [`PerCpu`] is owner-only. What other CPUs read is in [`PerCpuRemote`],
//! a separate per-CPU array whose fields are all atomics (DESIGN §7.5).

use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::apic::TimerMode;
use crate::desc::Tss;
use crate::sched::ReadyQueue;
use crate::thread::{CpuContext, GuardedStack, Tcb, ThreadId};

/// Dead threads' stacks one CPU keeps mapped for its next spawns, as
/// Linux's `NR_CACHED_STACKS`.
pub const STACK_CACHE_LEN: usize = 2;

/// A CPU's cache of dead threads' default-size kernel stacks, still mapped
/// (ROADMAP §10.10). Owner CPU only: the switch tail puts a stack its CPU
/// has switched off, and `spawn_inner` on that CPU takes one.
pub struct StackCache {
    slots: [Option<GuardedStack>; STACK_CACHE_LEN],
}

impl StackCache {
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; STACK_CACHE_LEN],
        }
    }

    /// Keep `stack`, or hand it back when the cache is full.
    #[allow(
        clippy::result_large_err,
        reason = "a refused stack comes back by value so its owner frees it once"
    )]
    pub fn put(&mut self, stack: GuardedStack) -> Result<(), GuardedStack> {
        match self.slots.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(stack);
                Ok(())
            }
            None => Err(stack),
        }
    }

    /// The stack put last, if any.
    pub fn take(&mut self) -> Option<GuardedStack> {
        self.slots.iter_mut().rev().find_map(Option::take)
    }

    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&self) -> bool {
        self.len() == STACK_CACHE_LEN
    }
}

impl Default for StackCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The part of one CPU's state that other CPUs read. One cache line per
/// CPU. Every field is atomic, so `&PerCpuRemote` may alias anything and
/// the type is `Sync` with no `unsafe impl` (`scripts/check_cells.py`).
#[repr(C, align(64))]
pub struct PerCpuRemote {
    /// Timer ticks on this CPU. Owner stores (single writer), Relaxed.
    pub ticks: AtomicU64,
    /// Context switches on this CPU. Owner stores (single writer), Relaxed.
    pub switches: AtomicU64,
    /// Length of the owner's `runq`, published at the end of every
    /// `&mut PerCpu` scope (`per_cpu_init::with_ptr`). Relaxed; a snapshot.
    pub runq_len: AtomicUsize,
    /// Set by the CPU after GS/IDT/LAPIC/timer, its last bring-up store
    /// (Release); the BSP waits on it (Acquire).
    pub ready: AtomicBool,
    /// ThreadId bitset. Remote CPUs OR a bit (Release) and send `0xFD`;
    /// the owner takes it with `swap` (Acquire).
    pub wake_inbox: AtomicU64,
    /// Local APIC id. Written before the CPU is started, then read-only.
    pub apic_id: AtomicU32,
    /// Root this CPU last loaded. Owner stores after each CR3 write
    /// (Release); `addr_space_init::teardown` reads it. 0 until paging
    /// publishes the kernel root.
    pub as_cr3: AtomicU64,
}

impl PerCpuRemote {
    pub const fn new() -> Self {
        Self {
            ticks: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            runq_len: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
            wake_inbox: AtomicU64::new(0),
            apic_id: AtomicU32::new(0),
            as_cr3: AtomicU64::new(0),
        }
    }
}

impl Default for PerCpuRemote {
    fn default() -> Self {
        Self::new()
    }
}

/// One CPU's local state, reached through `GS_BASE`. Owner-only: other
/// CPUs read [`PerCpuRemote`] through `remote` (DESIGN §7.5).
#[repr(C)]
pub struct PerCpu {
    pub self_ptr: *mut PerCpu,
    pub cpu_id: u32,
    pub irq_nest: AtomicU32,
    /// `ThreadId::NONE` until bootstrap / AP idle is installed.
    pub idle_id: ThreadId,
    pub current: *mut Tcb,
    pub idle: *mut Tcb,
    /// First TCB on this CPU's ready queue, or null.
    pub ready_head: *mut Tcb,
    pub tsc_per_ms: u64,
    /// TSC cycles spent in this CPU's idle thread.
    pub idle_tsc: u64,
    /// TSC at the start of the current slice.
    pub slice_tsc: u64,
    pub switch_scratch: CpuContext,
    pub timer_mode: TimerMode,
    /// Syscall entry and exit scratch, per CPU and valid only while IF=0:
    /// the user RSP, the syscall return value, and the `iretq` frame
    /// (DESIGN §7.5). `KERNEL_GS_BASE` holds `PerCpu` while CPL=3; see
    /// `arch::gs`.
    pub syscall_scratch: [u64; 6],
    /// Local ready FIFO. Owner CPU only, IRQs off. DESIGN §7.8.
    pub runq: ReadyQueue,
    /// Kernel stack top used by `syscall` and written into TSS.RSP0.
    pub kernel_rsp0: u64,
    /// Current CPU TSS. RSP0 updates go through here.
    pub tss: *mut Tss,
    /// Dedicated TSS stack from GDT init. Used when the TCB has no stack
    /// (bootstrap).
    pub fallback_rsp0: u64,
    /// The thread `switch_now` switched away from, for the switch tail
    /// (`thread_init::finish_switch`) that runs next on this CPU. Null
    /// outside that window.
    pub tail_prev: *mut Tcb,
    /// The stack of the thread that exited on this CPU and has not yet been
    /// switched off. `thread_exit` parks it; the next switch tail empties
    /// it, so it holds at most one.
    pub dead_stack: Option<GuardedStack>,
    /// Dead default-size stacks kept mapped for this CPU's next spawns.
    pub stack_cache: StackCache,
    /// Head of this CPU's list of dead stacks awaiting unmap, linked through
    /// the stacks themselves (`kva_init::park_on_list`); 0 when empty. This
    /// CPU's workqueue worker frees them with IF=1.
    pub dead_list: u64,
    /// This CPU's view in `per_cpu_init`'s separate array, the only
    /// per-CPU state another CPU reads.
    pub remote: &'static PerCpuRemote,
}

// SAFETY: invariant I120 and invariant I21, established at
// `per_cpu_init::cpu`, `per_cpu_init::with_current` and
// `per_cpu_init::with_cpu`: `per_cpu_init::CPUS` hands a `PerCpu` to its
// owner CPU alone (`with_current`, busy flag, IF=0), or through the
// `unsafe fn` `with_cpu` to the BSP while that CPU is not running; other
// CPUs read only the atomic `PerCpuRemote` that `remote` points to. The raw
// pointers it holds are owner-only and never dereferenced by another CPU.
unsafe impl Send for PerCpu {}
// SAFETY: as for `Send` above: invariant I120 and invariant I21,
// established at `per_cpu_init::cpu`, `per_cpu_init::with_current` and
// `per_cpu_init::with_cpu`.
unsafe impl Sync for PerCpu {}

impl PerCpu {
    pub const fn new(remote: &'static PerCpuRemote) -> Self {
        Self {
            self_ptr: core::ptr::null_mut(),
            cpu_id: 0,
            irq_nest: AtomicU32::new(0),
            idle_id: ThreadId::NONE,
            current: core::ptr::null_mut(),
            idle: core::ptr::null_mut(),
            ready_head: core::ptr::null_mut(),
            tsc_per_ms: 0,
            idle_tsc: 0,
            slice_tsc: 0,
            switch_scratch: CpuContext::empty(),
            timer_mode: TimerMode::Pit,
            syscall_scratch: [0; 6],
            runq: ReadyQueue::empty(),
            kernel_rsp0: 0,
            tss: core::ptr::null_mut(),
            fallback_rsp0: 0,
            tail_prev: core::ptr::null_mut(),
            dead_stack: None,
            stack_cache: StackCache::new(),
            dead_list: 0,
            remote,
        }
    }

    /// Store `runq.len()` into `remote.runq_len` (Relaxed). Called at the
    /// end of every `&mut PerCpu` scope; a run-queue change made outside
    /// one leaves the published length stale until the next scope ends.
    pub fn publish_runq_len(&self) {
        self.remote
            .runq_len
            .store(self.runq.len(), Ordering::Relaxed);
    }
}

const _: () = {
    assert!(offset_of!(PerCpu, self_ptr) == 0);
    assert!(offset_of!(PerCpu, cpu_id) == 8);
    assert!(offset_of!(PerCpu, idle_id) == 16);
    assert!(offset_of!(PerCpu, current) == 24);
    assert!(offset_of!(PerCpu, idle) == 32);
    assert!(offset_of!(PerCpu, ready_head) == 40);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_ptr_is_offset_zero() {
        static R: PerCpuRemote = PerCpuRemote::new();
        assert_eq!(offset_of!(PerCpu, self_ptr), 0);
        assert_eq!(offset_of!(PerCpu, cpu_id), 8);
        assert_eq!(offset_of!(PerCpu, idle_id), 16);
        assert_eq!(offset_of!(PerCpu, current), 24);
        assert_eq!(offset_of!(PerCpu, idle), 32);
        assert_eq!(offset_of!(PerCpu, ready_head), 40);
        let p = PerCpu::new(&R);
        assert!(p.idle_id.is_none());
        assert!(p.current.is_null());
        assert!(p.idle.is_null());
        assert!(p.ready_head.is_null());
        assert!(p.runq.is_empty());
        assert_eq!(p.timer_mode, TimerMode::Pit);
        assert!(core::ptr::eq(p.remote, &R));
        assert_eq!(p.syscall_scratch, [0; 6]);
        assert!(p.tss.is_null());
        assert_eq!(p.kernel_rsp0, 0);
        assert_eq!(p.remote.as_cr3.load(Ordering::Relaxed), 0);
        assert!(p.tail_prev.is_null());
        assert!(p.dead_stack.is_none());
        assert!(p.stack_cache.is_empty());
        assert_eq!(p.dead_list, 0);
        // The new owner-only fields sit after the pinned ones, before the
        // remote view (C-PERCPU layout).
        assert!(offset_of!(PerCpu, tail_prev) > offset_of!(PerCpu, fallback_rsp0));
        assert!(offset_of!(PerCpu, dead_list) < offset_of!(PerCpu, remote));
    }

    /// A stack handle over a range nothing maps, with no frames, so the
    /// test can drop it.
    fn fake_stack(guard: u64) -> GuardedStack {
        // SAFETY: no frames, and the handle is never unmapped or freed: the
        // test only moves it (the contract `GuardedStack::from_raw_parts`
        // states, met here).
        unsafe {
            GuardedStack::from_raw_parts(
                crate::paging::VirtAddr(guard),
                0,
                [const { None }; crate::thread::MAX_STACK_PAGES],
            )
        }
    }

    #[test]
    fn stack_cache_holds_two() {
        let mut c = StackCache::new();
        assert!(c.is_empty());
        assert!(c.take().is_none());
        assert!(c.put(fake_stack(0x1000)).is_ok());
        assert_eq!(c.len(), 1);
        assert!(!c.is_full());
        assert!(c.put(fake_stack(0x2000)).is_ok());
        assert!(c.is_full());
        match c.put(fake_stack(0x3000)) {
            Err(s) => assert_eq!(s.guard().as_u64(), 0x3000),
            Ok(()) => panic!("third stack kept"),
        }
        assert_eq!(c.len(), STACK_CACHE_LEN);
        assert_eq!(c.take().map(|s| s.guard().as_u64()), Some(0x2000));
        assert_eq!(c.take().map(|s| s.guard().as_u64()), Some(0x1000));
        assert!(c.take().is_none());
        assert!(c.is_empty());
        assert!(c.put(fake_stack(0x4000)).is_ok());
        assert_eq!(c.len(), 1);
        assert_eq!(StackCache::default().len(), 0);
    }

    #[test]
    fn remote_view_starts_clear() {
        static R: PerCpuRemote = PerCpuRemote::new();
        assert_eq!(core::mem::align_of::<PerCpuRemote>(), 64);
        assert_eq!(R.ticks.load(Ordering::Relaxed), 0);
        assert_eq!(R.switches.load(Ordering::Relaxed), 0);
        assert_eq!(R.runq_len.load(Ordering::Relaxed), 0);
        assert!(!R.ready.load(Ordering::Relaxed));
        assert_eq!(R.wake_inbox.load(Ordering::Relaxed), 0);
        assert_eq!(R.apic_id.load(Ordering::Relaxed), 0);
        assert_eq!(R.as_cr3.load(Ordering::Relaxed), 0);
        let d = PerCpuRemote::default();
        assert_eq!(d.ticks.load(Ordering::Relaxed), 0);
        assert!(!d.ready.load(Ordering::Relaxed));

        let mut p = PerCpu::new(&R);
        p.publish_runq_len();
        assert_eq!(R.runq_len.load(Ordering::Relaxed), 0);
        p.runq.push_back(ThreadId(3));
        p.runq.push_back(ThreadId(4));
        assert_eq!(R.runq_len.load(Ordering::Relaxed), 0);
        p.publish_runq_len();
        assert_eq!(R.runq_len.load(Ordering::Relaxed), 2);
        assert!(p.runq.remove(ThreadId(3)));
        p.publish_runq_len();
        assert_eq!(R.runq_len.load(Ordering::Relaxed), 1);
    }
}
