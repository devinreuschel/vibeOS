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

#[cfg(feature = "panic_stop_test")]
pub(crate) use stop::{after_panic_message, stop_trip};

/// F135: the stop primitive's four ways, at `-smp 5` (DESIGN §2.5 step 1).
/// CPUs 0 and 1 panic at once; CPU 2 prints a numbered line in a loop with
/// IF=0; CPU 3 waits with IF=0 on a `SpinMutex` CPU 0 holds; CPU 4 spins
/// with IF=0 and neither polls nor writes. The harness's `check_stop`
/// reads how each stopped.
#[cfg(feature = "panic_stop_test")]
mod stop {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use vibeos::apic::IpiMode;
    use vibeos::lock::RANK_DEVICE;

    use crate::sync_init::SpinMutex;
    use crate::{apic_init, ipi_init, per_cpu_init, thread_init, time_init, x86};

    /// CPUs the scenario needs: the panicking pair and one per other way.
    const CPUS: u32 = 5;
    /// How long CPU 0 waits for the four threads to be in place.
    const READY_MS: u64 = 10_000;
    /// How long the owner waits for its own NMI to come back.
    const SELF_NMI_MS: u64 = 10;
    /// CPU 2's lines before it counts as in place.
    const LINES_BEFORE_READY: u64 = 3;

    static READY: [AtomicBool; CPUS as usize] = [const { AtomicBool::new(false) }; CPUS as usize];
    static GO: AtomicBool = AtomicBool::new(false);
    /// Set once CPU 0 has spawned the four threads and holds `HELD`. Each
    /// thread waits for it with IF=1 before its `cli`: until then a spawn's
    /// TLB shootdown needs every CPU to acknowledge, and CPU 3 must not take
    /// `HELD` first.
    static GATE: AtomicBool = AtomicBool::new(false);
    static SPIN: AtomicU64 = AtomicU64::new(0);
    /// Held by CPU 0 from before `GO` through its panic.
    static HELD: SpinMutex<()> = SpinMutex::with_rank((), RANK_DEVICE);

    /// Wait for `GATE` with IF=1 (a thread starts with IF on), then `cli`.
    fn enter() {
        // Acquire: pairs with CPU 0's Release store of `GATE`.
        while !GATE.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        x86::cli();
    }

    fn ready(cpu: usize) {
        if let Some(r) = READY.get(cpu) {
            // Release: pairs with CPU 0's Acquire wait in `stop_trip`.
            r.store(true, Ordering::Release);
        }
    }

    #[allow(
        clippy::panic,
        reason = "the test-only panic_stop_test feature's purpose: CPU 1 panics beside CPU 0"
    )]
    fn cpu1() {
        enter();
        ready(1);
        // Acquire: pairs with CPU 0's Release store of `GO`. No poll.
        while !GO.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        panic!("panic-stop: cpu 1");
    }

    fn cpu2() {
        enter();
        let mut n = 0u64;
        loop {
            crate::marker!("vibeOS: panic_stop: line {}", n);
            n = n.wrapping_add(1);
            if n == LINES_BEFORE_READY {
                ready(2);
            }
        }
    }

    fn cpu3() {
        enter();
        ready(3);
        // CPU 0 holds it until the panic: a serviced spin with IF=0.
        let _g = HELD.lock();
        x86::halt();
    }

    fn cpu4() {
        enter();
        ready(4);
        loop {
            SPIN.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Wait up to `ms` for `f`; whether it held.
    fn wait_ms(ms: u64, f: impl Fn() -> bool) -> bool {
        let per_ms = time_init::tsc_per_ms().max(1);
        let start = time_init::read_tsc();
        while !f() {
            if time_init::read_tsc().wrapping_sub(start) / per_ms >= ms {
                return false;
            }
            core::hint::spin_loop();
        }
        true
    }

    /// Set the scene on CPUs 1 to 4, then panic on CPU 0 with `HELD` held.
    #[allow(
        clippy::panic,
        reason = "the test-only panic_stop_test feature's purpose: prove the stop primitive end to end"
    )]
    pub(crate) fn stop_trip() {
        let online = per_cpu_init::online_mask().count_ones();
        if online < CPUS {
            panic!("panic-stop: needs {CPUS} online CPUs, found {online}");
        }
        crate::marker!("vibeOS: boot: panic-stop armed");
        let entries: [(u32, fn()); 4] = [(1, cpu1), (2, cpu2), (3, cpu3), (4, cpu4)];
        for (cpu, entry) in entries {
            if thread_init::spawn_on("panic-stop", entry, cpu).is_err() {
                panic!("panic-stop: spawn on cpu {cpu} failed");
            }
        }
        // After the spawns, which take the heap and SCHED locks, ranked
        // below `HELD`.
        let _held = HELD.lock();
        GATE.store(true, Ordering::Release);
        // Acquire: pairs with each thread's Release store in `ready`.
        let all = || READY.iter().skip(1).all(|r| r.load(Ordering::Acquire));
        if !wait_ms(READY_MS, all) {
            panic!("panic-stop: cpus 1-4 not in place");
        }
        // Release: CPU 1 panics once it sees it.
        GO.store(true, Ordering::Release);
        panic!("panic-stop: cpu 0");
    }

    /// The dump owner, after its panic message: an NMI to its own APIC id
    /// (a physical destination: the self shorthand takes Fixed delivery
    /// only), then the owner line once the NMI body returned.
    pub(crate) fn after_panic_message() {
        let before = ipi_init::OWNER_NMI_RETURNS.load(Ordering::Relaxed);
        let Some(me) = per_cpu_init::cpu(crate::arch::cpu_id_hint()) else {
            return;
        };
        let apic = me.apic_id.load(Ordering::Relaxed) as u8;
        // A refused NMI leaves out the owner line, which fails the run.
        if apic_init::send_ipi(apic, 0, IpiMode::Nmi).is_err() {
            return;
        }
        let back = || ipi_init::OWNER_NMI_RETURNS.load(Ordering::Relaxed) != before;
        if wait_ms(SELF_NMI_MS, back) {
            crate::serial::raw::write_owner(b"vibeOS: panic_stop: owner nmi returned");
        }
    }
}
