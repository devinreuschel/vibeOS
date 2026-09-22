//! Syscall numbers and the Phase 9A entry stub. ROADMAP §9.1 / §9.3.
//!
//! Slice A owns the entry/exit path and a stub that returns `ENOSYS` for
//! every number. The numbered dispatch table, arity, and per-call
//! validation land in Slice B.

/// Linux `ENOSYS`.
pub const ENOSYS: i32 = 38;

/// Slice A stub: any number is "not implemented". Negative errno in rax.
#[inline]
pub fn stub(_nr: u64) -> i64 {
    -(ENOSYS as i64)
}

/// Asm entry calls this. No table.
#[unsafe(no_mangle)]
pub extern "C" fn vibeos_syscall_stub(nr: u64) -> i64 {
    stub(nr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_is_enosys_for_any_number() {
        for nr in [0u64, 1, 60, 999, u64::MAX] {
            assert_eq!(stub(nr), -(ENOSYS as i64));
            assert_eq!(vibeos_syscall_stub(nr), -(ENOSYS as i64));
        }
        assert_eq!(ENOSYS, 38);
    }
}
