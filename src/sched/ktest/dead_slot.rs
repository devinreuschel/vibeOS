//! `lifetime_dead_slot_on_cpu`: a Dead TCB slot is reused only once its
//! CPU has switched off it (ROADMAP §10.10, F012).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::thread::ThreadId;

use super::{STALL_EXITS, STALL_MS, TAIL_CPUS, WAIT_NS, tail_cpus_online};
use crate::ktest::Outcome;
use crate::sync::blocking_init::Semaphore;
use crate::thread_init::{self, SpawnError};
use crate::time_init;

/// Dead slots left for CPU 0's exits once the fillers hold the rest.
const FREE_SLOTS: u32 = 4;

/// Spawns each spawner on CPUs 1 to 3 makes.
const SPAWNS_PER_CPU: u32 = 400;

static FILL_LIVE: AtomicU32 = AtomicU32::new(0);

/// Where the fillers wait: one `release` lets one go. They block rather
/// than poll, so a full table of them (ROADMAP §10.4) leaves CPU 0 free
/// for the exits the test times.
static FILL_PARK: Semaphore = Semaphore::new(0);

/// Fillers spawned, and releases given to [`FILL_PARK`], this run.
static FILL_SPAWNED: AtomicU32 = AtomicU32::new(0);
static FILL_FREED: AtomicU32 = AtomicU32::new(0);

static SPAWN_GO: AtomicBool = AtomicBool::new(false);

static SPAWNERS_DONE: AtomicU32 = AtomicU32::new(0);

static SPAWN_BAD: AtomicBool = AtomicBool::new(false);

/// Tid buckets [`SPAWNED`] and [`RAN`] count in: any number works, since
/// a tid lands in the same bucket in both.
const TID_BUCKETS: usize = 256;

/// Per tid bucket ([`tid_bucket`]): spawns that returned a tid in it, and
/// runs of a child with one. Tids are not reused before the allocator
/// wraps, so a child that ran twice or not at all shows as a mismatch.
static SPAWNED: [AtomicU32; TID_BUCKETS] = [const { AtomicU32::new(0) }; TID_BUCKETS];

static RAN: [AtomicU32; TID_BUCKETS] = [const { AtomicU32::new(0) }; TID_BUCKETS];

/// Hold a TCB slot, blocked on CPU 0, until released.
fn filler_entry() {
    FILL_LIVE.fetch_add(1, Ordering::AcqRel);
    FILL_PARK.acquire();
    FILL_LIVE.fetch_sub(1, Ordering::AcqRel);
}

/// Let `n` more fillers go.
fn free_fillers(n: u32) {
    let mut i = 0u32;
    while i < n {
        FILL_PARK.release();
        i += 1;
    }
    FILL_FREED.fetch_add(n, Ordering::AcqRel);
}

/// `id`'s bucket in [`SPAWNED`] and [`RAN`].
fn tid_bucket(id: ThreadId) -> usize {
    id.raw() as usize % TID_BUCKETS
}

fn child_entry() {
    if let Some(r) = RAN.get(tid_bucket(thread_init::current_id())) {
        r.fetch_add(1, Ordering::AcqRel);
    }
}

/// On CPUs 1 to 3: spawn children onto CPU 0 as fast as slots come free.
fn slot_spawner() {
    while !SPAWN_GO.load(Ordering::Acquire) {
        thread_init::yield_now();
    }
    let t0 = time_init::now_ns();
    let mut n = 0u32;
    while n < SPAWNS_PER_CPU {
        match thread_init::spawn_on("child", child_entry, 0) {
            Ok(h) => {
                if let Some(s) = SPAWNED.get(tid_bucket(h.id())) {
                    s.fetch_add(1, Ordering::AcqRel);
                }
                n += 1;
            }
            // A free slot comes with CPU 0's next exit, and KVA for a stack
            // with its worker's next free: `Kva::alloc` refuses once
            // `MAX_KVA_RANGES - 2` ranges are live, and the burst parks
            // dead stacks on CPU 0's dead list faster than it frees them.
            Err(SpawnError::NoSlot | SpawnError::NoMemory) => thread_init::yield_now(),
        }
        if time_init::now_ns().saturating_sub(t0) > 6 * WAIT_NS {
            SPAWN_BAD.store(true, Ordering::Release);
            break;
        }
    }
    SPAWNERS_DONE.fetch_add(1, Ordering::AcqRel);
}

fn wait_for(pred: impl Fn() -> bool, ns: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ns {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Let every filler go and wait for them, bounded.
fn release_fillers() -> bool {
    let left = FILL_SPAWNED
        .load(Ordering::Acquire)
        .saturating_sub(FILL_FREED.load(Ordering::Acquire));
    free_fillers(left);
    wait_for(|| FILL_LIVE.load(Ordering::Acquire) == 0, WAIT_NS)
}

/// A spawn reuses a `Dead` TCB slot only once its CPU has switched off it
/// (ROADMAP §10.10, F012): fillers hold every slot but four, so the only
/// Dead slots are CPU 0's exits, held open for 10 ms on the first 100;
/// spawners on CPUs 1 to 3 respawn into them, and every child runs its
/// entry exactly once.
pub(crate) fn lifetime_dead_slot_on_cpu() -> Outcome {
    if !tail_cpus_online() {
        return Outcome::Skip("needs 4 cpus");
    }
    for (s, r) in SPAWNED.iter().zip(RAN.iter()) {
        s.store(0, Ordering::Relaxed);
        r.store(0, Ordering::Relaxed);
    }
    FILL_LIVE.store(0, Ordering::Release);
    FILL_SPAWNED.store(0, Ordering::Release);
    FILL_FREED.store(0, Ordering::Release);
    SPAWN_GO.store(false, Ordering::Release);
    SPAWNERS_DONE.store(0, Ordering::Release);
    SPAWN_BAD.store(false, Ordering::Release);

    // The spawners take their slots before the fillers take the rest.
    let mut spawners = 0u32;
    for cpu in 1..TAIL_CPUS {
        if let Err(e) = thread_init::spawn_on("slot-spawner", slot_spawner, cpu) {
            SPAWN_GO.store(true, Ordering::Release);
            SPAWN_BAD.store(true, Ordering::Release);
            let _done = wait_for(
                || SPAWNERS_DONE.load(Ordering::Acquire) == spawners,
                WAIT_NS,
            );
            return crate::fail_fmt!("spawner: {}", e.as_str());
        }
        spawners += 1;
    }
    // Fill every free slot, then let four fillers exit on CPU 0.
    let mut fillers = 0u32;
    loop {
        match thread_init::spawn_on("filler", filler_entry, 0) {
            Ok(_) => {
                fillers += 1;
                FILL_SPAWNED.fetch_add(1, Ordering::AcqRel);
            }
            Err(SpawnError::NoSlot) => break,
            Err(SpawnError::NoMemory) => {
                SPAWN_GO.store(true, Ordering::Release);
                let _released = release_fillers();
                return Outcome::Fail("filler spawn: no memory");
            }
        }
    }
    let fill_target = fillers;
    if !wait_for(|| FILL_LIVE.load(Ordering::Acquire) == fill_target, WAIT_NS) {
        SPAWN_GO.store(true, Ordering::Release);
        let _released = release_fillers();
        return Outcome::Fail("fillers did not start");
    }
    free_fillers(FREE_SLOTS.min(fillers));
    let left = fill_target.saturating_sub(FREE_SLOTS);
    if !wait_for(|| FILL_LIVE.load(Ordering::Acquire) == left, WAIT_NS) {
        SPAWN_GO.store(true, Ordering::Release);
        let _released = release_fillers();
        return Outcome::Fail("fillers did not exit");
    }

    thread_init::testing::arm_exit_stall(0, STALL_EXITS, STALL_MS);
    SPAWN_GO.store(true, Ordering::Release);
    let done = wait_for(
        || SPAWNERS_DONE.load(Ordering::Acquire) == spawners,
        6 * WAIT_NS,
    );
    let sum = |a: &[AtomicU32]| a.iter().map(|x| x.load(Ordering::Acquire)).sum::<u32>();
    let ran = wait_for(|| sum(&RAN) >= sum(&SPAWNED), WAIT_NS);
    thread_init::testing::disarm_exit_stall();
    let released = release_fillers();
    if !done {
        return Outcome::Fail("spawners did not finish");
    }
    if SPAWN_BAD.load(Ordering::Acquire) {
        return Outcome::Fail("a spawner gave up");
    }
    for (bucket, (s, r)) in SPAWNED.iter().zip(RAN.iter()).enumerate() {
        let (s, r) = (s.load(Ordering::Acquire), r.load(Ordering::Acquire));
        if s != r {
            return crate::fail_fmt!("tid bucket {bucket}: spawned {s}, ran {r}");
        }
    }
    if !ran {
        return Outcome::Fail("children did not run");
    }
    if !released {
        return Outcome::Fail("fillers did not exit at the end");
    }
    Outcome::Ok
}
