//! In-guest tests of P10-S05, Cell bounds and the PerCpu remote view (DESIGN §8.2).

use core::sync::atomic::Ordering;

use super::{Outcome, Test, cpu_remote, spin_until_ns, test};
use crate::per_cpu_init;
use crate::x86;

pub(super) const TESTS: &[Test] = &[test("percpu_remote_view", percpu_remote_view)];

/// How long [`percpu_remote_view`] waits for this CPU's `ticks` to move.
const TICK_WAIT_NS: u64 = 200_000_000;

/// `per_cpu_init::cpu` hands out each CPU's `PerCpuRemote`, and the owner
/// keeps its fields current: `ready` and distinct `apic_id`s on every
/// online CPU, `runq_len` after a `with_current` scope, and `ticks` with
/// IF on.
fn percpu_remote_view() -> Outcome {
    let n = per_cpu_init::cpu_count();
    if n == 0 {
        return Outcome::Fail("cpu array empty");
    }
    let mut i = 0u32;
    while (i as usize) < n {
        if cpu_remote(i).is_none() {
            return Outcome::Fail("cpu(i) is None below cpu_count");
        }
        i += 1;
    }
    if cpu_remote(n as u32).is_some() {
        return Outcome::Fail("cpu(cpu_count) is Some");
    }

    let mut a = 0u32;
    while (a as usize) < n {
        if per_cpu_init::is_online(a) {
            let Some(ra) = cpu_remote(a) else {
                return Outcome::Fail("online cpu has no view");
            };
            if !ra.ready.load(Ordering::Acquire) {
                return Outcome::Fail("online cpu not ready");
            }
            let mut b = a + 1;
            while (b as usize) < n {
                if per_cpu_init::is_online(b)
                    && cpu_remote(b).is_some_and(|rb| {
                        rb.apic_id.load(Ordering::Relaxed) == ra.apic_id.load(Ordering::Relaxed)
                    })
                {
                    return Outcome::Fail("two online cpus share an apic_id");
                }
                b += 1;
            }
        }
        a += 1;
    }

    {
        let _g = x86::InterruptGuard::enter();
        let me = per_cpu_init::current();
        if !cpu_remote(me.cpu_id).is_some_and(|r| core::ptr::eq(r, me.remote)) {
            return Outcome::Fail("cpu(me) is not PerCpu.remote");
        }
        let len = per_cpu_init::with_current(|pc| pc.runq.len());
        if me.remote.runq_len.load(Ordering::Relaxed) != len {
            return Outcome::Fail("runq_len != runq.len()");
        }
    }

    if !x86::interrupts_enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    let t0 = per_cpu_init::current().remote.ticks.load(Ordering::Relaxed);
    // The registry thread is pinned to CPU 0, so `current()` stays this CPU.
    let moved = spin_until_ns(
        || per_cpu_init::current().remote.ticks.load(Ordering::Relaxed) != t0,
        TICK_WAIT_NS,
    );
    if !moved {
        return Outcome::Fail("ticks did not advance with IF on");
    }
    Outcome::Ok
}
