//! The bootstrap thread: boot's own context made thread 0 and moved off
//! Limine's stack onto a guarded KVA stack (ROADMAP §10.6, MEMORY.md §4.5).

use core::ops::Range;
use core::sync::atomic::AtomicU32;

use vibeos::arch::ContextSwitch;
use vibeos::desc::UserSegs;
use vibeos::kalloc::TryBox;
use vibeos::proc::INIT_PID;
use vibeos::thread::{
    CpuAffinity, CpuContext, Fxsave, OnCpu, Tcb, ThreadId, ThreadState, WaitOutcome,
};

use super::{SCHED, tid_of_slot, with_sched};
use crate::arch::current::Arch;
use crate::kva_init;
use crate::per_cpu_init;

/// The bootstrap thread's stack. x86_64: 64 KiB (ROADMAP §10.2). aarch64:
/// 16 KiB, the one size every kernel stack uses (ROADMAP §11.3).
#[cfg(target_arch = "aarch64")]
pub(crate) const BOOT_STACK_PAGES: usize = 4;
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const BOOT_STACK_PAGES: usize = 16;

/// Make boot thread 0 and move it off Limine's stack: allocate its guarded
/// [`BOOT_STACK_PAGES`] KVA stack, adopt it as the running bootstrap thread
/// with `stack` set, and switch onto that stack into `rest`, which never
/// returns. Nothing ever resumes the frames on Limine's stack. After
/// [`per_cpu_init::init_bsp`].
///
/// # Safety
/// Single-CPU, IF=0, `GS_BASE` live, KVA up, not already initialized.
pub unsafe fn init_bootstrap(rest: extern "C" fn() -> !) -> ! {
    // Before `irq: enabled`, where DESIGN §4.4 allows a boot-time halt.
    // The stack is part of the bootstrap TCB, so it shares the TCB's line.
    let Ok(stack) = kva_init::alloc_guarded_stack(BOOT_STACK_PAGES) else {
        crate::boot::halt_with("vibeOS: thread: no memory for the bootstrap TCB");
    };
    let top = stack.top().as_u64();
    let tcb = TryBox::try_new(Tcb {
        id: ThreadId::BOOTSTRAP,
        name: "bootstrap",
        state: ThreadState::Running,
        on_cpu: OnCpu::new_set(),
        stack: Some(stack),
        context: CpuContext::empty(),
        entry: bootstrap_entry,
        affinity: CpuAffinity::Pinned(0),
        cpu: 0,
        irq_nest: 0,
        switches: 0,
        run_tsc: 0,
        wait_outcome: WaitOutcome::Woken,
        as_cr3: 0,
        fpu: Fxsave::INITIAL,
        fp_cpu: None,
        user_segs: UserSegs::NULL,
        syscall_count: vibeos::atomic::AtomicU64::new(0),
        pid: 0,
        no_reclaim: AtomicU32::new(0),
    });
    let Ok(mut tcb) = tcb else {
        crate::boot::halt_with("vibeOS: thread: no memory for the bootstrap TCB");
    };
    let ptr = &mut *tcb as *mut Tcb;
    {
        let mut s = SCHED.lock();
        assert!(s.slots[0].is_none(), "bootstrap twice");
        s.slots[0] = Some(tcb);
        // Tid 0 is the bootstrap's, which `PidAlloc` never hands out.
        s.bind(ThreadId::BOOTSTRAP, 0);
        // Init stays pid 1: held before any other thread takes an id, and
        // taken over by init's process (`proc_init::alloc_pid`).
        assert!(s.hold_id(INIT_PID), "pid: init's hold");
    }
    crate::ipi_init::set_slot_tid_hook(tid_of_slot);
    per_cpu_init::with_current(|cpu| {
        per_cpu_init::set_current_thread(cpu, ptr);
        cpu.idle = ptr;
        cpu.idle_id = ThreadId::BOOTSTRAP;
    });
    // The one context-switch primitive (AGENTS rule 10): `from` takes the
    // Limine-stack context, which nothing resumes; the bootstrap TCB's own
    // `context` is written by its first switch away.
    let mut from = CpuContext::empty();
    let mut to = CpuContext::empty();
    Arch::prepare(&mut to, top, rest as *const () as u64);
    // SAFETY: IF=0 (this fn's `# Safety` contract); `from` is a local
    // written here, and `to` was filled by `prepare` over the fresh guarded
    // stack the bootstrap TCB owns, mapped by `kva_init::alloc_guarded_stack`
    // and live for as long as the bootstrap thread runs; established here.
    unsafe { Arch::switch(&mut from, &to) };
    #[allow(
        clippy::panic,
        reason = "invariant: `from` is a local that no TCB holds, so no switch resumes it (`thread_init::init_bootstrap`)"
    )]
    {
        panic!("bootstrap: Limine stack resumed");
    }
}

#[allow(
    clippy::panic,
    reason = "invariant: the bootstrap TCB is adopted Running and never started, so nothing enters its `entry` (`thread_init::init_bootstrap`)"
)]
fn bootstrap_entry() {
    panic!("bootstrap entry called");
}

/// The bootstrap thread's stack range, its saved RSP, and whether it is on
/// a CPU now (its saved RSP is stale while it runs). `None` before
/// [`init_bootstrap`].
#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "x86-only on the boot-CPU slice")
)]
pub(crate) fn bootstrap_stack() -> Option<(Range<u64>, u64, bool)> {
    with_sched(|s| {
        let t = s.get(ThreadId::BOOTSTRAP)?;
        let st = t.stack.as_ref()?;
        Some((
            st.base().as_u64()..st.top().as_u64(),
            t.context.stack_ptr(),
            !t.on_cpu.is_clear(),
        ))
    })
}
