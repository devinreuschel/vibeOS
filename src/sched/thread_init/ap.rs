//! AP idle threads: adopted for an AP that starts on its own stack, and
//! retired, with any other thread that never ran, when bring-up fails
//! (ROADMAP §10.4, F037).

use super::*;

/// AP idle: running on `stack` already. No synthetic frame, not on the FIFO.
/// `Err(stack)` hands the stack back when no TCB slot is free. A reusable
/// Dead slot is rewritten in place, as `spawn_inner` does; only when none
/// is free does a new `Tcb` box go into an empty slot. That box is
/// allocated before SCHED, and one that finds no slot is handed back out of
/// `with_sched` and dropped after it: no heap free under SCHED.
#[allow(
    clippy::result_large_err,
    reason = "the stack comes back by value so the caller frees it once; a Box would allocate on the failure path"
)]
pub fn adopt_ap_idle(cpu_id: u32, stack: GuardedStack) -> Result<ThreadId, GuardedStack> {
    let reused = with_sched(move |s| {
        let Some(slot) = s.slots.iter().position(|x| slot_reusable(x.as_deref())) else {
            return Err(stack);
        };
        let Some(old) = s.slots.get(slot).and_then(|x| x.as_deref()).map(|t| t.id) else {
            return Err(stack);
        };
        let Some(raw) = s.alloc_id() else {
            return Err(stack);
        };
        let id = ThreadId(raw);
        s.unbind(old);
        let Some(tcb) = s.slots.get_mut(slot).and_then(|x| x.as_deref_mut()) else {
            s.free_id(raw);
            return Err(stack);
        };
        tcb.id = id;
        fill_ap_idle(tcb, cpu_id);
        tcb.stack = Some(stack);
        s.bind(id, slot);
        Ok(id)
    });
    let stack = match reused {
        Ok(id) => return Ok(id),
        Err(stack) => stack,
    };
    // Built without the stack, which the AP is running on: a failed
    // allocation must not drop it.
    let tcb = TryBox::try_new(Tcb {
        id: ThreadId(0),
        name: "idle",
        state: ThreadState::Running,
        on_cpu: OnCpu::new_set(),
        stack: None,
        context: CpuContext::empty(),
        entry: ap_idle_entry,
        affinity: CpuAffinity::Pinned(cpu_id),
        cpu: cpu_id,
        irq_nest: 0,
        switches: 0,
        run_tsc: 0,
        wait_outcome: WaitOutcome::Woken,
        as_cr3: 0,
        fpu: fpu_template(),
        fp_cpu: None,
        syscall_count: 0,
        pid: 0,
        no_reclaim: AtomicU32::new(0),
    });
    let Ok(mut tcb) = tcb else {
        return Err(stack);
    };
    let placed = with_sched(|s| {
        let Some(slot) = s.slots.iter().position(|x| x.is_none()) else {
            // Dropped after SCHED is released: no heap free under it.
            return Err((tcb, stack));
        };
        let Some(raw) = s.alloc_id() else {
            return Err((tcb, stack));
        };
        let id = ThreadId(raw);
        tcb.id = id;
        tcb.stack = Some(stack);
        s.slots[slot] = Some(tcb);
        s.bind(id, slot);
        Ok(id)
    });
    match placed {
        Ok(id) => Ok(id),
        Err((tcb, stack)) => {
            drop(tcb);
            Err(stack)
        }
    }
}

/// Rewrite a reused Dead TCB as CPU `cpu_id`'s AP idle thread: Running on
/// the stack the AP starts on, as [`adopt_ap_idle`]'s new box is built.
fn fill_ap_idle(tcb: &mut Tcb, cpu_id: u32) {
    tcb.name = "idle";
    tcb.state = ThreadState::Running;
    tcb.on_cpu.set();
    tcb.context = CpuContext::empty();
    tcb.entry = ap_idle_entry;
    tcb.affinity = CpuAffinity::Pinned(cpu_id);
    tcb.cpu = cpu_id;
    tcb.irq_nest = 0;
    tcb.switches = 0;
    tcb.run_tsc = 0;
    tcb.wait_outcome = WaitOutcome::Woken;
    tcb.as_cr3 = 0;
    tcb.fpu = fpu_template();
    // A reused TCB address: no CPU's `fp_owner` may match it.
    fp_invalidate(tcb);
    tcb.syscall_count = 0;
    tcb.pid = 0;
    tcb.no_reclaim.store(0, Ordering::Relaxed);
}

/// Retire thread `id`, which was never made runnable (an AP's idle thread
/// or a worker parked for a CPU that did not come up), and return its
/// stack for the caller to free. `may_have_run`: an AP that got a SIPI may
/// have started on its idle thread past the ready timeout and may still
/// run on it, so its slot stays unreusable; otherwise nothing ever ran on
/// the TCB and the slot is free for a later spawn.
pub fn abandon_unstarted(id: ThreadId, may_have_run: bool) -> Option<GuardedStack> {
    with_sched(|s| {
        s.timeouts.remove(id);
        let t = s.get_mut(id)?;
        t.state = ThreadState::Dead;
        let stack = t.stack.take();
        if !may_have_run {
            // Release: nothing ran on the TCB, so this publishes only the
            // Dead state `spawn_inner`'s Acquire load pairs with.
            t.on_cpu.clear();
        }
        stack
    })
}

#[allow(
    clippy::panic,
    reason = "invariant: an AP idle TCB is adopted Running and never started, so nothing enters its `entry` (`thread_init::adopt_ap_idle`)"
)]
fn ap_idle_entry() {
    panic!("ap idle entry called");
}
