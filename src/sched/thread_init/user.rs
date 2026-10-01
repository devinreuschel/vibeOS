//! A user thread's ring-3 CPU state outside the context switch: what `fork`
//! and `execve` set in its TCB (DESIGN §5.1, §7.5).

use super::*;

/// Give `id`, a user thread not yet [`make_ready`], ring-3 selectors `segs`
/// for its first entry (`fork`: the parent's, DESIGN §5.1).
pub fn set_user_segs(id: ThreadId, segs: UserSegs) {
    with_sched(|s| {
        if let Some(t) = s.get_mut(id) {
            t.user_segs = segs;
        }
    });
}

/// `execve`'s selectors: the running thread's `Tcb.user_segs` and the live
/// DS, ES, FS and GS become null, as Linux's `execve` leaves them (DESIGN
/// §5.1). Before the new image's `FS_BASE` write, which a null FS load may
/// zero.
pub fn reset_user_segs() {
    let _irq = InterruptGuard::enter();
    let t = crate::arch::current_tcb();
    if t.is_null() {
        return;
    }
    // SAFETY: invariant: `current_tcb` is the TCB this CPU runs, which only
    // this CPU touches while it runs, and the guard's IF=0 keeps it current
    // and meets `load_user_segs`'s `# Safety` with the null selectors;
    // established by `thread_init::switch_now`.
    unsafe {
        (*t).user_segs = UserSegs::NULL;
        crate::arch::gdt::load_user_segs(UserSegs::NULL);
    }
}
