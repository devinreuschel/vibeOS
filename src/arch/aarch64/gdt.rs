//! No GDT on aarch64. Facade so `main.rs` keeps one boot order.

use vibeos::desc::UserSegs;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::per_cpu::PerCpu;

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub struct CpuTables;
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub struct ApTables;

/// # Safety
/// No-op: aarch64 has no GDT.
pub unsafe fn init_bsp() {}

pub fn read_user_segs() -> UserSegs {
    UserSegs::NULL
}

pub fn load_user_segs(_s: UserSegs) {}

/// This CPU's overflow stack, for the panic backtrace.
pub fn this_cpu_stacks(_cpu: &PerCpu) -> [(u64, usize); 1] {
    let top = super::cpu::overflow_sp();
    if top == 0 {
        [(0, 0)]
    } else {
        [(top, DEFAULT_STACK_PAGES)]
    }
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn alloc_ap_tables() -> Option<ApTables> {
    None
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn free_ap_tables(_t: ApTables) {}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn bsp_tables() -> *mut CpuTables {
    core::ptr::null_mut()
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn bsp_rsp0_top() -> u64 {
    0
}
