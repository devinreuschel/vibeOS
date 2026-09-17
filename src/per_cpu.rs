//! Per-CPU struct layout. DESIGN §7.5.
//!
//! `self_ptr` is at offset 0 so `gs:[0]` yields the struct address.
//! Hardware install (`GS_BASE`) lives in the binary crate.
//!
//! Slice A installs the BSP only. No `MAX_CPUS` array, no swapgs.
//! `idle` is the bootstrap TCB until Slice B's `sti; hlt` idle.
//! `ready_head` is null until Slice B; the UP TCB table is the queue.

use core::mem::offset_of;

use crate::thread::{CpuContext, Tcb, ThreadId};

/// BSP / AP local state. Wake inbox / timer mode / heap array: phase 4.
#[repr(C)]
pub struct PerCpu {
    pub self_ptr: *mut PerCpu,
    pub cpu_id: u32,
    pub apic_id: u32,
    pub irq_nest: u32,
    /// `ThreadId::NONE` until bootstrap is installed.
    pub idle_id: ThreadId,
    pub current: *mut Tcb,
    pub idle: *mut Tcb,
    /// Ready-list head. Null = empty. Slice B fills this (or points it
    /// at the first ready TCB from the UP global table).
    pub ready_head: *mut Tcb,
    pub tsc_per_ms: u64,
    pub ticks: u64,
    pub switches: u64,
    pub switch_scratch: CpuContext,
    pub _reserved: [u64; 8],
}

impl PerCpu {
    pub const fn empty() -> Self {
        Self {
            self_ptr: core::ptr::null_mut(),
            cpu_id: 0,
            apic_id: 0,
            irq_nest: 0,
            idle_id: ThreadId::NONE,
            current: core::ptr::null_mut(),
            idle: core::ptr::null_mut(),
            ready_head: core::ptr::null_mut(),
            tsc_per_ms: 0,
            ticks: 0,
            switches: 0,
            switch_scratch: CpuContext::empty(),
            _reserved: [0; 8],
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
    }
}
