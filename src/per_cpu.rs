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
use crate::thread::{CpuContext, Tcb, ThreadId};

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
    /// CR3 this CPU last loaded. 0 until paging publishes the kernel root.
    pub as_cr3: u64,
    /// Dedicated TSS stack from GDT init. Used when the TCB has no stack
    /// (bootstrap).
    pub fallback_rsp0: u64,
    /// This CPU's view in `per_cpu_init`'s separate array, the only
    /// per-CPU state another CPU reads.
    pub remote: &'static PerCpuRemote,
}

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
            as_cr3: 0,
            fallback_rsp0: 0,
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
        assert_eq!(p.as_cr3, 0);
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
