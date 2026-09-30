//! Test-only panic-path trips (ROADMAP §10.7): each builds its own ISO,
//! whose e2e mode requires one clean dump and the panic exit. No
//! published ISO enables them (AGENTS.md rule 9).

/// F071: underflow `irq_nest` through the decrement `InterruptGuard::drop`
/// uses, with this CPU's depth at 0. The underflow's `assert!` panics, and
/// with the depth wrapped every later guard fails it again, so the dump
/// must write without one.
#[cfg(feature = "panic_nest_test")]
pub(crate) fn nest_trip() {
    crate::marker!("vibeOS: boot: panic-nest armed");
    // IF=0, as in the guard's own `drop`, which reads this CPU's slot.
    crate::x86::cli();
    crate::per_cpu_init::irq_nest_leave();
}
