//! Per-CPU struct layout. DESIGN §7.5.
//!
//! `self_ptr` is at offset 0 so `gs:[0]` yields the struct address.
//! Hardware install (`GS_BASE`) lives in the binary crate.

use crate::thread::CpuContext;

/// BSP / AP local state. Ready queue, idle, timer mode land in phase 4.
#[repr(C)]
pub struct PerCpu {
    pub self_ptr: *mut PerCpu,
    pub cpu_id: u32,
    pub apic_id: u32,
    pub irq_nest: u32,
    _pad: u32,
    /// `Tcb` in the kernel. Untyped here so the library does not own stacks.
    pub current: *mut u8,
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
            _pad: 0,
            current: core::ptr::null_mut(),
            tsc_per_ms: 0,
            ticks: 0,
            switches: 0,
            switch_scratch: CpuContext::empty(),
            _reserved: [0; 8],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    #[test]
    fn self_ptr_is_offset_zero() {
        assert_eq!(offset_of!(PerCpu, self_ptr), 0);
        assert_eq!(offset_of!(PerCpu, cpu_id), 8);
        assert_eq!(offset_of!(PerCpu, current), 24);
    }
}
