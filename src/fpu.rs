//! The FP register binding: which thread's FP and vector state each CPU's
//! registers hold, and when it is saved and restored (DESIGN §7.5).
//!
//! Each CPU has an `fp_owner`, the address of the thread whose state it
//! last loaded ([`NO_OWNER`] for none), and each thread an `fp_cpu`, the
//! CPU it last loaded on. A CPU's registers hold thread T's state only
//! when both agree ([`is_live`]). The switch away from a live thread
//! saves it and loads nothing; every return to user mode loads the
//! thread's saved state unless it is live, then binds both fields. A new
//! thread and a thread whose saved state was written start unbound, so a
//! reused TCB address or a stale owner never matches. Addresses are
//! compared and never dereferenced. Pure state transitions; the kernel
//! half does the saves and loads.

/// `fp_owner` of a CPU whose registers hold no thread's state.
pub const NO_OWNER: usize = 0;

/// What the switch away from a thread does with the FP registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchAway {
    /// They hold the thread's state: save it into the thread.
    Save,
    /// They hold someone else's state, or nothing that is the thread's.
    Nothing,
}

/// What a return to user mode does with the FP registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserReturn {
    /// They already hold the thread's state.
    Keep,
    /// Load the thread's saved state, then [`bind`].
    Load,
}

/// The registers of `cpu`, whose owner is `fp_owner`, hold the state of
/// the thread at address `t`, whose `fp_cpu` is `t_fp_cpu`.
pub fn is_live(fp_owner: usize, cpu: u32, t: usize, t_fp_cpu: Option<u32>) -> bool {
    t != NO_OWNER && fp_owner == t && t_fp_cpu == Some(cpu)
}

/// The switch away from thread `t` on `cpu`: save only live state.
pub fn switch_away(fp_owner: usize, cpu: u32, t: usize, t_fp_cpu: Option<u32>) -> SwitchAway {
    if is_live(fp_owner, cpu, t, t_fp_cpu) {
        SwitchAway::Save
    } else {
        SwitchAway::Nothing
    }
}

/// A return to user mode of thread `t` on `cpu`: load unless live.
pub fn user_return(fp_owner: usize, cpu: u32, t: usize, t_fp_cpu: Option<u32>) -> UserReturn {
    if is_live(fp_owner, cpu, t, t_fp_cpu) {
        UserReturn::Keep
    } else {
        UserReturn::Load
    }
}

/// After loading thread `t`'s state on `cpu`: both fields name each other.
pub fn bind(fp_owner: &mut usize, cpu: u32, t: usize, t_fp_cpu: &mut Option<u32>) {
    *fp_owner = t;
    *t_fp_cpu = Some(cpu);
}

/// The thread's saved state was written: no CPU's registers hold it now.
pub fn invalidate(t_fp_cpu: &mut Option<u32>) {
    *t_fp_cpu = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One CPU's owner field.
    struct Cpu {
        id: u32,
        owner: usize,
    }

    /// One thread: its TCB address and `fp_cpu`.
    struct Thread {
        addr: usize,
        fp_cpu: Option<u32>,
    }

    impl Cpu {
        fn new(id: u32) -> Self {
            Self {
                id,
                owner: NO_OWNER,
            }
        }

        /// A return to user mode of `t` here; true when it loaded.
        fn ret(&mut self, t: &mut Thread) -> bool {
            match user_return(self.owner, self.id, t.addr, t.fp_cpu) {
                UserReturn::Keep => false,
                UserReturn::Load => {
                    bind(&mut self.owner, self.id, t.addr, &mut t.fp_cpu);
                    true
                }
            }
        }

        fn away(&self, t: &Thread) -> SwitchAway {
            switch_away(self.owner, self.id, t.addr, t.fp_cpu)
        }
    }

    #[test]
    fn migration_loads_on_new_cpu_and_on_return() {
        let (mut a, mut b) = (Cpu::new(0), Cpu::new(1));
        let mut t = Thread {
            addr: 0x1000,
            fp_cpu: None,
        };
        assert!(a.ret(&mut t));
        assert!(!a.ret(&mut t), "live on A: kept");
        assert_eq!(a.away(&t), SwitchAway::Save);
        // Runs on B next.
        assert!(b.ret(&mut t));
        assert_eq!(a.away(&t), SwitchAway::Nothing, "A no longer holds it");
        assert_eq!(b.away(&t), SwitchAway::Save);
        // On A again, whose owner still names t: its registers are stale.
        assert_eq!(a.owner, t.addr);
        assert!(a.ret(&mut t));
        assert!(!is_live(b.owner, b.id, t.addr, t.fp_cpu));
    }

    #[test]
    fn reused_tcb_address_never_matches_stale_owner() {
        let mut a = Cpu::new(0);
        let mut old = Thread {
            addr: 0x2000,
            fp_cpu: None,
        };
        assert!(a.ret(&mut old));
        // `old` dies; a new thread gets the same TCB address.
        let mut new = Thread {
            addr: 0x2000,
            fp_cpu: None,
        };
        assert_eq!(a.away(&new), SwitchAway::Nothing);
        assert!(a.ret(&mut new), "new thread must load its own state");
    }

    #[test]
    fn write_to_saved_state_loads_and_is_not_saved_over() {
        let mut a = Cpu::new(0);
        let mut t = Thread {
            addr: 0x3000,
            fp_cpu: None,
        };
        assert!(a.ret(&mut t));
        invalidate(&mut t.fp_cpu);
        assert_eq!(
            a.away(&t),
            SwitchAway::Nothing,
            "the live registers must not overwrite the written state"
        );
        assert!(a.ret(&mut t), "the written state is loaded");
    }

    #[test]
    fn kernel_thread_is_never_live() {
        let mut a = Cpu::new(0);
        let mut u = Thread {
            addr: 0x4000,
            fp_cpu: None,
        };
        let k = Thread {
            addr: 0x5000,
            fp_cpu: None,
        };
        assert!(a.ret(&mut u));
        // A kernel thread never returns to user mode, so never binds.
        assert_eq!(a.away(&k), SwitchAway::Nothing);
        assert!(!is_live(a.owner, a.id, k.addr, k.fp_cpu));
        assert!(!is_live(NO_OWNER, 0, NO_OWNER, Some(0)));
        // The user thread's state survived the kernel thread's run.
        assert!(!a.ret(&mut u));
    }
}
