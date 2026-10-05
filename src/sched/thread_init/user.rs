//! A user thread's ring-3 CPU state outside the context switch: what `fork`
//! and `execve` set in its TCB (DESIGN §5.1, §7.5).

use super::*;

/// The image a new TCB starts from (DESIGN §7.5). x86_64 uses the psABI
/// FXSAVE image; aarch64 uses V0–V31, FPCR, and FPSR zero, because
/// `fp_load` stores those registers at the start of `bytes`.
pub(crate) fn initial_fxsave() -> Fxsave {
    #[cfg(target_arch = "aarch64")]
    {
        Fxsave::ZERO
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        Fxsave::INITIAL
    }
}

/// Give `id`, a user thread not yet [`make_ready`], ring-3 selectors `segs`
/// for its first entry (`fork`: the parent's, DESIGN §5.1).
pub fn set_user_segs(id: ThreadId, segs: UserSegs) {
    with_sched(|s| {
        if let Some(t) = s.get_mut(id) {
            t.user_segs = segs;
        }
    });
}

/// Give `id` its saved user TLS base before [`make_ready`].
pub fn set_tls_base(id: ThreadId, tls: u64) {
    with_sched(|s| {
        if let Some(t) = s.get_mut(id) {
            t.tls_base = tls;
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

impl Sched {
    /// The CPU `id` runs on, when it is `Running` there: where a signal's
    /// sender sends the reschedule IPI that makes the target's exit work
    /// see it (DESIGN §5.10 rule 11).
    pub(crate) fn running_on(&self, id: ThreadId) -> Option<u32> {
        self.get(id)
            .filter(|t| t.state == ThreadState::Running)
            .map(|t| t.cpu)
    }
}
