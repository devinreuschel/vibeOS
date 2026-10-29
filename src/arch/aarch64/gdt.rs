//! No GDT on aarch64. Facade so `main.rs` keeps one boot order.
//! `ApTables` holds this CPU's overflow stack.

use vibeos::desc::UserSegs;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::per_cpu::PerCpu;

use crate::kva_init::{self, GuardedStack};

pub struct CpuTables;

pub struct ApTables {
    overflow: Option<GuardedStack>,
}

impl ApTables {
    pub fn overflow_top(&self) -> u64 {
        self.overflow
            .as_ref()
            .map(|s| s.top().as_u64())
            .unwrap_or(0)
    }
}

/// # Safety
/// No-op: aarch64 has no GDT.
pub unsafe fn init_bsp() {}

pub fn read_user_segs() -> UserSegs {
    UserSegs::NULL
}

pub fn load_user_segs(_s: UserSegs) {}

/// This CPU's overflow stack, for the panic backtrace.
pub fn this_cpu_stacks(cpu: &PerCpu) -> [(u64, usize); 1] {
    let top = cpu.overflow_sp;
    if top == 0 {
        [(0, 0)]
    } else {
        [(top, DEFAULT_STACK_PAGES)]
    }
}

pub fn alloc_ap_tables() -> Option<ApTables> {
    let overflow = kva_init::alloc_guarded_stack(DEFAULT_STACK_PAGES).ok()?;
    Some(ApTables {
        overflow: Some(overflow),
    })
}

pub fn free_ap_tables(t: ApTables) {
    if let Some(s) = t.overflow {
        kva_init::free_stack(s);
    }
}

#[expect(dead_code, reason = "x86 syscall_init facade; unused on aarch64")]
pub fn bsp_tables() -> *mut CpuTables {
    core::ptr::null_mut()
}

#[expect(dead_code, reason = "x86 syscall_init facade; unused on aarch64")]
pub fn bsp_rsp0_top() -> u64 {
    0
}
