//! BSP scheduler bring-up, idle thread, timer preemption. ROADMAP §3.3, §3.6, §4.8.
//!
//! Marker `sched: cpu0 ready` after idle exists. PIT/LAPIC call [`on_timer_tick`]
//! after EOI (DESIGN §5.8). Idle is always runnable, never on the FIFO.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::per_cpu_init;
use crate::thread_init;

static LIVE: AtomicBool = AtomicBool::new(false);

/// Spawn the BSP idle thread, retarget `PerCpu.idle`, arm preemption.
///
/// # Safety
/// Call once, on the BSP, after `thread_init::init_bootstrap` and before
/// `irq: enabled`, with the bootstrap thread current: it rewrites this
/// CPU's `PerCpu.idle`, which the switch path reads. IRQ0 may already be
/// live; `on_timer_tick` is a no-op until `LIVE`.
pub unsafe fn init() {
    // Before `irq: enabled`: a failure halts (DESIGN §4.4's boot policy).
    let Ok(h) = thread_init::spawn_idle(idle_main) else {
        crate::boot::halt_with("vibeOS: sched: no idle thread");
    };
    let ptr = thread_init::tcb_ptr(h.id());
    assert!(!ptr.is_null(), "idle tcb");
    per_cpu_init::with_current(|cpu| {
        cpu.idle = ptr;
        cpu.idle_id = h.id();
        cpu.slice_tsc = crate::time_init::read_tsc();
        cpu.quantum_tsc = cpu.slice_tsc;
    });
    crate::ipi_init::set_reschedule_hook(thread_init::schedule_preempt);
    thread_init::start_sweep();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    per_cpu_init::arm_if_checks();
    LIVE.store(true, Ordering::Release);
}

pub fn is_live() -> bool {
    LIVE.load(Ordering::Acquire)
}

/// After EOI. Preempt a thread that has run `QUANTUM_MS` of TSC time, or
/// at every tick while idle so a sleeper can displace `sti; hlt`. Each CPU
/// owns its runq.
pub fn on_timer_tick() {
    #[cfg(feature = "kernel_tests")]
    crate::ktest::on_tick();
    if !is_live() {
        return;
    }
    crate::work_init::kick_deferred();
    let quantum = crate::time_init::tsc_per_ms().saturating_mul(vibeos::sched::QUANTUM_MS);
    let now = crate::time_init::read_tsc();
    let preempt = per_cpu_init::with_current(|cpu| {
        // Single writer: only this CPU stores its `ticks`.
        let ticks = cpu.remote.ticks.load(Ordering::Relaxed).wrapping_add(1);
        cpu.remote.ticks.store(ticks, Ordering::Relaxed);
        let idle = crate::arch::current_tcb() == cpu.idle && !cpu.idle.is_null();
        vibeos::sched::should_preempt(now.wrapping_sub(cpu.quantum_tsc), quantum, idle)
    });
    if preempt {
        thread_init::schedule_preempt();
    }
}

/// Shared idle body for BSP and APs. DESIGN §7.8.
pub fn idle_loop() -> ! {
    loop {
        thread_init::yield_now();
        thread_init::halt_if_idle();
    }
}

fn idle_main() {
    idle_loop();
}

#[cfg(feature = "kernel_tests")]
pub fn idle_tsc() -> u64 {
    per_cpu_init::with_current(|c| c.idle_tsc)
}
