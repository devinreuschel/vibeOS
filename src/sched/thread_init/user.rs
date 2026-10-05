//! A user thread's ring-3 CPU state outside the context switch: what `fork`
//! and `execve` set in its TCB (DESIGN §5.1, §7.5).

use core::ptr;

use super::*;

/// Box a TCB by writing each field through the allocation. `Tcb` holds
/// `MAX_STACK_PAGES` frame tokens and an `Fxsave`; a by-value constructor
/// would put that frame under every spawn (DESIGN §4.5).
#[inline(never)]
#[allow(clippy::too_many_arguments)] // same fields as the TCB constructor
pub(super) fn box_new_tcb(
    name: &'static str,
    entry: fn(),
    affinity: CpuAffinity,
    enqueue: bool,
    cpu: u32,
    first_nest: u32,
    pid: u32,
    as_cr3: u64,
) -> Result<TryBox<Tcb>, AllocError> {
    let slot = TryBox::<Tcb>::try_new_uninit()?;
    let raw = TryBox::into_raw(slot);
    // SAFETY: `raw` is the exclusive `MaybeUninit<Tcb>` `try_new_uninit`
    // allocated. Each field is written once, then the pointer is a live
    // `Tcb`; established here.
    unsafe {
        let p = raw.cast::<Tcb>();
        ptr::addr_of_mut!((*p).id).write(ThreadId(0));
        ptr::addr_of_mut!((*p).name).write(name);
        ptr::addr_of_mut!((*p).state).write(if enqueue { ThreadState::Ready } else { PARKED });
        ptr::addr_of_mut!((*p).on_cpu).write(OnCpu::new());
        ptr::addr_of_mut!((*p).stack).write(None);
        ptr::addr_of_mut!((*p).context).write(CpuContext::empty());
        ptr::addr_of_mut!((*p).entry).write(entry);
        ptr::addr_of_mut!((*p).affinity).write(affinity);
        ptr::addr_of_mut!((*p).cpu).write(cpu);
        ptr::addr_of_mut!((*p).irq_nest).write(first_nest);
        ptr::addr_of_mut!((*p).switches).write(0);
        ptr::addr_of_mut!((*p).run_tsc).write(0);
        ptr::addr_of_mut!((*p).wait_outcome).write(WaitOutcome::Woken);
        ptr::addr_of_mut!((*p).as_cr3).write(as_cr3);
        ptr::addr_of_mut!((*p).fpu).write(initial_fxsave());
        ptr::addr_of_mut!((*p).fp_cpu).write(None);
        ptr::addr_of_mut!((*p).user_segs).write(UserSegs::NULL);
        ptr::addr_of_mut!((*p).tls_base).write(0);
        ptr::addr_of_mut!((*p).syscall_count).write(vibeos::atomic::AtomicU64::new(0));
        ptr::addr_of_mut!((*p).pid).write(pid);
        ptr::addr_of_mut!((*p).no_reclaim).write(AtomicU32::new(0));
        Ok(TryBox::from_raw(p))
    }
}

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
