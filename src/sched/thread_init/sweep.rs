//! The blocked-thread sweep (DESIGN §6.5, ROADMAP §10.7): a kernel thread
//! pinned to CPU 0 that every `SWEEP_TICKS` ticks scans the thread table
//! for threads still waiting `OVERDUE_NS` past their recorded deadline
//! (`vibeos::sched::find_overdue`) and prints
//! `vibeOS: sched: overdue tid <id>` for each. The timeout path wakes such
//! a thread at its deadline, so a report means its timeout entry was lost
//! or never queued.
//!
//! The scan holds SCHED, with IF off, for `SWEEP_CHUNK` TCBs at a time and
//! yields between chunks: over the whole table it measured above ROADMAP
//! §10.7's 20 µs under TCG (`sched_sweep_cost`, TIME.md §6.5). No priority
//! class exists yet, so yielding is how it stays low-priority. It prints
//! from thread context, after each hold.

use vibeos::sched::{OVERDUE_REPORT, SWEEP_TICKS, find_overdue};
use vibeos::thread::{MAX_THREADS, ThreadId};
use vibeos::time::Instant;

use super::{Sched, spawn_on, with_sched};
use crate::time_init;

/// TCB slots one SCHED hold scans.
pub(crate) const SWEEP_CHUNK: usize = 64;

/// Start the sweep thread on CPU 0. From `sched_init::init`, once the
/// scheduler's tables and idle thread exist. The sweep is a diagnostic,
/// so a failed spawn is logged and boot goes on.
pub fn start_sweep() {
    if let Err(e) = spawn_on("sched-sweep", sweep_main, 0) {
        crate::klog!(
            vibeos::log::Level::Error,
            "sched: sweep thread not started: {}",
            e.as_str()
        );
    }
}

fn sweep_main() {
    loop {
        // The tick is ~1 kHz (DESIGN §6.1), so this is `SWEEP_TICKS` ticks.
        super::sleep_ms(SWEEP_TICKS);
        sweep_once();
    }
}

/// One sweep over the whole table, a chunk per SCHED hold.
fn sweep_once() {
    let mut base = 0usize;
    while base < MAX_THREADS {
        let now = Instant {
            ns: time_init::now_ns(),
        };
        let mut from = ThreadId(0);
        loop {
            let mut out = [ThreadId::NONE; OVERDUE_REPORT];
            let (n, next) = with_sched(|s| scan_chunk(s, base, now, from, &mut out));
            // A failure path, printed with SCHED dropped.
            for id in out.iter().take(n) {
                crate::marker!("vibeOS: sched: overdue tid {}", id.raw());
                #[cfg(feature = "kernel_tests")]
                super::testing::overdue_printed(*id);
            }
            // A full buffer: more overdue threads past the last one in
            // this chunk, which the next pass starts after.
            if n < OVERDUE_REPORT || next.raw() == 0 {
                break;
            }
            from = next;
        }
        base = base.saturating_add(SWEEP_CHUNK);
        super::yield_now();
    }
    #[cfg(feature = "kernel_tests")]
    super::testing::sweep_done();
}

/// The overdue threads among slots `base..base + SWEEP_CHUNK` whose tid is
/// at least `from`, smallest tids first, into `out`: how many, and the tid
/// after the last. Under SCHED.
pub(super) fn scan_chunk(
    s: &Sched,
    base: usize,
    now: Instant,
    from: ThreadId,
    out: &mut [ThreadId],
) -> (usize, ThreadId) {
    let end = base.saturating_add(SWEEP_CHUNK).min(s.slots.len());
    let Some(chunk) = s.slots.get(base..end) else {
        return (0, from);
    };
    let f = from.raw();
    let threads = chunk
        .iter()
        .flatten()
        .map(|t| (t.id, t.state))
        .filter(move |(id, _)| id.raw() >= f);
    find_overdue(threads, now, from, out)
}
