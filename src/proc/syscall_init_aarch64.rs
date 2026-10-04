//! aarch64 syscall / switch hooks. User `svc` is ROADMAP §11.6.

use vibeos::per_cpu::PerCpu;
use vibeos::syscall::UserFrame;
use vibeos::thread::{Tcb, ThreadId};

use crate::per_cpu_init;

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub const EXIT_SYSCALL: u64 = 0x100;

pub fn set_exit_work_hooks(_pending: fn() -> bool, _work: fn(&mut UserFrame)) {}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn exit_work(_kind: u64, _frame: &mut UserFrame) {}

/// # Safety
/// Unused on the boot-CPU kernel-only slice.
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn init_cpu() {}

/// # Safety
/// TPIDR is the BSP `PerCpu`.
pub unsafe fn init_bsp() {
    crate::thread_init::set_switch_hooks(on_switch);
    per_cpu_init::with_current(|cpu| {
        // Release: pairs with the Acquire load in `addr_space_init::root_holder`.
        cpu.remote.as_cr3.store(
            crate::paging_init::kernel_cr3(),
            core::sync::atomic::Ordering::Release,
        );
    });
    crate::log_init::apply_boot_level();
}

/// # Safety
/// Unused: no secondary cores in this slice.
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn init_ap(_tables: *const crate::arch::gdt::CpuTables, _rsp0: u64) {}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn fp_save(_tcb: &mut Tcb) {}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn current_fp_words() -> Option<(u32, u32, u32)> {
    None
}

pub fn fork_fp(_child: ThreadId) {}

pub fn exec_fp() {}

/// # Safety
/// `cpu` is this CPU's `PerCpu`.
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn set_rsp0_for(_cpu: &mut PerCpu, _tcb: &Tcb) {}

/// # Safety
/// `cpu` is this CPU's `PerCpu`.
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn switch_cr3_for(_cpu: &mut PerCpu, _tcb: &Tcb) -> bool {
    false
}

/// # Safety
/// As `on_switch`.
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn switch_fpu(_cpu: &mut PerCpu, _old: *mut Tcb) {}

/// # Safety
/// As `thread_init::switch_now`.
pub unsafe fn on_switch(_cpu: &mut PerCpu, _old: *mut Tcb, _new: *mut Tcb) {}

/// # Safety
/// User entry is ROADMAP §11.6.
pub unsafe fn first_return(_fs_base: u64) -> ! {
    crate::arch::current::halt();
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn set_trace(_on: bool) {}

pub fn trace_enabled() -> bool {
    false
}

pub fn set_syscall_handler(_f: fn(&mut UserFrame) -> i64) {}

#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub(crate) mod testing {
    pub(crate) fn arm_noncanonical_rip(_nr: u64) {}
    pub(crate) fn disarm_noncanonical() {}
    pub(crate) fn arm_noncanonical_entry() {}
    pub(crate) fn bad_rip_kills() -> u64 {
        0
    }
    pub(crate) fn arm_fork_wait_stall(_n: u32) {}
}
