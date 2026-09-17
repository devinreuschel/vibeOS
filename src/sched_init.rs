//! BSP scheduler bring-up, idle thread, timer preemption. ROADMAP §3.3, §3.6.
//!
//! Marker `sched: cpu0 ready` after idle exists. PIT calls [`on_timer_tick`]
//! after EOI (DESIGN §5.8). Idle is always runnable, never on the FIFO.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::per_cpu_init;
use crate::thread_init;

static LIVE: AtomicBool = AtomicBool::new(false);

/// Spawn the BSP idle thread, retarget `PerCpu.idle`, arm preemption.
///
/// # Safety
/// Bootstrap thread is current. IRQ0 may already be live; `on_timer_tick`
/// is a no-op until `LIVE`.
pub unsafe fn init() {
    let h = thread_init::spawn_idle(idle_main);
    let ptr = thread_init::tcb_ptr(h.id());
    assert!(!ptr.is_null(), "idle tcb");
    let cpu = per_cpu_init::current_mut();
    cpu.idle = ptr;
    cpu.idle_id = h.id();
    cpu.slice_tsc = crate::time_init::read_tsc();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    LIVE.store(true, Ordering::Release);
}

pub fn is_live() -> bool {
    LIVE.load(Ordering::Acquire)
}

/// After EOI. Preempt every `QUANTUM_TICKS`, or every tick while idle
/// so a sleeper can displace `sti; hlt`.
pub fn on_timer_tick() {
    if !is_live() {
        return;
    }
    let cpu = per_cpu_init::current_mut();
    cpu.ticks = cpu.ticks.wrapping_add(1);
    let idle = cpu.current == cpu.idle && !cpu.idle.is_null();
    if vibeos::sched::should_preempt(cpu.ticks, idle) {
        thread_init::schedule_preempt();
    }
}

fn idle_main() {
    loop {
        // Timer preempt resumes idle with from_irq, so schedule skips
        // reap. Drain here: not on a dying stack, not on the IRQ path.
        thread_init::reap_zombies();
        thread_init::yield_now();
        // `sti; hlt` is one instruction pair so IF cannot open a window
        // before halt. Always runnable, lowest priority (not on the FIFO).
        unsafe {
            asm!("sti; hlt", options(nomem, nostack));
        }
    }
}

#[allow(dead_code)]
pub fn idle_tsc() -> u64 {
    per_cpu_init::current().idle_tsc
}
