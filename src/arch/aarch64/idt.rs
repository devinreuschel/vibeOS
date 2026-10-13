//! Vector-table facade under the `idt` name shared kernel code already uses.

#[expect(unused_imports, reason = "facade matches x86 `idt` names")]
pub use super::vectors::{
    TrapFrame, init, load, registrable, set_handler, set_intercept_hook, set_user_fault_hook,
    set_user_return_hook, user_fault,
};

/// After the bootstrap thread is on a guarded stack.
pub use super::vectors::init_full;

/// Test hooks. x86's live in `idt.rs`; aarch64 user tests are §11.6.
#[cfg(feature = "kernel_tests")]
pub mod testing {
    #[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
    pub static GPR_CANARIES: [u64; 15] = [0; 15];

    pub fn hits() -> u64 {
        0
    }
    pub fn bad() -> u64 {
        0
    }
    pub fn misplaced() -> u64 {
        0
    }
    pub fn arm_canaries(_a: u64, _b: u64, _c: u64, _n: u64) {}
    pub fn disarm_canaries() {}
    pub fn arm_db_repin(_a: u64, _b: u64) {}
    pub fn disarm_db_repin() {}
    pub fn repin_cpus() -> (u32, u32) {
        (0, 0)
    }
    pub fn arm_pf_yield(_cr2: u64) {}
    pub fn disarm_pf_yield() {}
    pub fn pf_during_yield() -> u64 {
        0
    }
}
