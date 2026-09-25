//! Per-CPU struct layout. DESIGN §7.5.
//!
//! `self_ptr` is at offset 0 so `gs:[0]` yields the struct address.
//! Hardware install (`GS_BASE`) lives in the binary crate.
//!
//! Heap array sized from the MADT CPU count (no `MAX_CPUS` static).
//! `runq` is this CPU's ready FIFO; remotes use `wake_inbox` + IPI.

use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

use crate::apic::TimerMode;
use crate::desc::Tss;
use crate::sched::ReadyQueue;
use crate::thread::{CpuContext, Tcb, ThreadId};

/// One CPU's local state, reached through `GS_BASE`.
#[repr(C)]
pub struct PerCpu {
    pub self_ptr: *mut PerCpu,
    pub cpu_id: u32,
    pub apic_id: u32,
    pub irq_nest: AtomicU32,
    /// `ThreadId::NONE` until bootstrap / AP idle is installed.
    pub idle_id: ThreadId,
    pub current: *mut Tcb,
    pub idle: *mut Tcb,
    /// First TCB on this CPU's ready queue, or null.
    pub ready_head: *mut Tcb,
    pub tsc_per_ms: u64,
    pub ticks: u64,
    pub switches: u64,
    /// TSC cycles spent in this CPU's idle thread.
    pub idle_tsc: u64,
    /// TSC at the start of the current slice.
    pub slice_tsc: u64,
    pub switch_scratch: CpuContext,
    /// ThreadId bitset. Remote CPUs OR a bit and send `0xFD`.
    pub wake_inbox: AtomicU64,
    pub timer_mode: TimerMode,
    /// Set by the AP after GS/IDT/LAPIC/timer; BSP waits on this.
    pub ready: AtomicBool,
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
}

impl PerCpu {
    pub const fn empty() -> Self {
        Self {
            self_ptr: core::ptr::null_mut(),
            cpu_id: 0,
            apic_id: 0,
            irq_nest: AtomicU32::new(0),
            idle_id: ThreadId::NONE,
            current: core::ptr::null_mut(),
            idle: core::ptr::null_mut(),
            ready_head: core::ptr::null_mut(),
            tsc_per_ms: 0,
            ticks: 0,
            switches: 0,
            idle_tsc: 0,
            slice_tsc: 0,
            switch_scratch: CpuContext::empty(),
            wake_inbox: AtomicU64::new(0),
            timer_mode: TimerMode::Pit,
            ready: AtomicBool::new(false),
            syscall_scratch: [0; 6],
            runq: ReadyQueue::empty(),
            kernel_rsp0: 0,
            tss: core::ptr::null_mut(),
            as_cr3: 0,
            fallback_rsp0: 0,
        }
    }
}

const _: () = {
    assert!(offset_of!(PerCpu, self_ptr) == 0);
    assert!(offset_of!(PerCpu, cpu_id) == 8);
    assert!(offset_of!(PerCpu, idle_id) == 20);
    assert!(offset_of!(PerCpu, current) == 24);
    assert!(offset_of!(PerCpu, idle) == 32);
    assert!(offset_of!(PerCpu, ready_head) == 40);
};

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    #[test]
    fn self_ptr_is_offset_zero() {
        assert_eq!(offset_of!(PerCpu, self_ptr), 0);
        assert_eq!(offset_of!(PerCpu, cpu_id), 8);
        assert_eq!(offset_of!(PerCpu, idle_id), 20);
        assert_eq!(offset_of!(PerCpu, current), 24);
        assert_eq!(offset_of!(PerCpu, idle), 32);
        assert_eq!(offset_of!(PerCpu, ready_head), 40);
        let p = PerCpu::empty();
        assert!(p.idle_id.is_none());
        assert!(p.current.is_null());
        assert!(p.idle.is_null());
        assert!(p.ready_head.is_null());
        assert!(p.runq.is_empty());
        assert_eq!(p.timer_mode, TimerMode::Pit);
        assert!(!p.ready.load(Ordering::Relaxed));
        assert_eq!(p.wake_inbox.load(Ordering::Relaxed), 0);
        assert_eq!(p.syscall_scratch, [0; 6]);
        assert!(p.tss.is_null());
        assert_eq!(p.kernel_rsp0, 0);
        assert_eq!(p.as_cr3, 0);
    }
}
