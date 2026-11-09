//! aarch64 ASID proof (ROADMAP §11.2, PORTABILITY §11.2): 1,000 short-lived
//! processes under the `kernel_tests` 4-bit cap, each writing and checking
//! a private pattern at one shared VA while its CPU switches among them.
//!
//! A boot that selects [`CAP`] runs with 15 usable ASIDs
//! (`arch::aarch64::asid::init_bsp`), so the processes force rollovers on
//! every CPU while other CPUs run theirs. Each CPU keeps [`PER_CPU`]
//! processes alive at once and they yield to each other, so a switch goes
//! from one process straight to another. TCG keeps a CPU's soft TLB across
//! a TTBR0 write that leaves the ASID unchanged, so an ASID that named two
//! live spaces on one CPU between its flushes would hand one process the
//! other's frame, and its check would fail. TCG drops that TLB whenever the
//! ASID changes, so a missing rollover flush shows only in the kernel's
//! record of each CPU's ASID-to-space pairs since its last flush
//! (`arch::aarch64::asid::aliases`), which the test also checks, and in
//! guest memory under HVF.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use vibeos::thread::ThreadId;

use crate::arch::aarch64::asid;
use crate::ktest::user::{self, Image, Layout, user_code};
use crate::ktest::{Outcome, sleep_until, spawn_thread_on};
use crate::per_cpu_init;
use crate::thread_init;

/// The registry row the 4-bit cap serves.
const CAP: &str = asid::CAP_TEST;
/// The processes this test runs in all.
const PROCS: u32 = 1_000;
/// Processes alive at once on each CPU.
const PER_CPU: usize = 5;
/// Code at the first page; the pattern goes in the two after it.
const LAYOUT: Layout = Layout {
    vaddr: 0x4000_0000,
    memsz: Some(0x3000),
    writable: true,
};

// Pattern: pid in bits 63:32 over a constant, plus the round. Thirty-two
// words at a 256-byte stride over the two pages after the code page,
// written, then one `sched_yield`, then checked, twenty rounds. Exit 0 when
// every check held, 1 at the first word that did not.
user_code!(
    ASID_PATTERN,
    "
    mov x8, #172
    svc #0
    lsl x20, x0, #32
    movz x9, #0xA5C3
    movk x9, #0x5A3C, lsl #16
    orr x20, x20, x9
    adr x21, .
    lsr x21, x21, #12
    lsl x21, x21, #12
    add x21, x21, #0x1000
    mov w19, #20
1:
    mov x22, x21
    mov x23, #32
2:
    eor x24, x20, x23
    str x24, [x22]
    add x22, x22, #256
    subs x23, x23, #1
    b.ne 2b
    mov x8, #124
    svc #0
    mov x22, x21
    mov x23, #32
3:
    ldr x25, [x22]
    eor x24, x20, x23
    cmp x25, x24
    b.ne 9f
    add x22, x22, #256
    subs x23, x23, #1
    b.ne 3b
    add x20, x20, #1
    subs w19, w19, #1
    b.ne 1b
    mov x0, xzr
    mov x8, #93
    svc #0
9:
    mov x0, #1
    mov x8, #93
    svc #0
    "
);

/// Processes claimed so far, shared by every CPU's spawner; a claim at or
/// past [`PROCS`] starts nothing.
static CLAIMED: AtomicU32 = AtomicU32::new(0);
static PASSED: AtomicU32 = AtomicU32::new(0);
/// The first failure's wait status, or `u64::MAX - 1` for a spawn error.
static FIRST_BAD: AtomicU64 = AtomicU64::new(u64::MAX);
static SPAWNERS_DONE: AtomicU32 = AtomicU32::new(0);

fn note_bad(v: u64) {
    // AcqRel: the first failure wins; pairs with the Acquire load in
    // `test_asid_procs_4bit`. Relaxed: the failure load; pairs with nothing.
    let _ = FIRST_BAD.compare_exchange(u64::MAX, v, Ordering::AcqRel, Ordering::Relaxed);
}

/// Claim up to `n` of the processes not yet started.
fn take(n: usize) -> usize {
    let mut got = 0;
    // Relaxed: each claim is one unique index; pairs with nothing.
    while got < n && CLAIMED.fetch_add(1, Ordering::Relaxed) < PROCS {
        got += 1;
    }
    got
}

/// One CPU's spawner: batches of [`PER_CPU`] processes, which run pinned
/// here (`thread_init::spawn_user` pins to the spawner's CPU), until none
/// are left.
fn spawner() {
    loop {
        let n = take(PER_CPU);
        if n == 0 {
            break;
        }
        let mut pids = [0u32; PER_CPU];
        for slot in pids.iter_mut().take(n) {
            match user::spawn(&Image::Code(ASID_PATTERN, LAYOUT), &["asid"]) {
                Ok(pid) => *slot = pid,
                Err(_) => note_bad(u64::MAX - 1),
            }
        }
        for &pid in pids.iter().take(n) {
            if pid == 0 {
                continue;
            }
            let st = user::wait(pid);
            if vibeos::proc::wifexited(st) && vibeos::proc::wexitstatus(st) == 0 {
                // Relaxed: a count read after `SPAWNERS_DONE`'s Acquire; pairs with nothing.
                PASSED.fetch_add(1, Ordering::Relaxed);
            } else {
                note_bad(u64::from(st));
            }
        }
    }
    // Release: pairs with the Acquire load in `test_asid_procs_4bit`.
    SPAWNERS_DONE.fetch_add(1, Ordering::Release);
}

pub(crate) fn test_asid_procs_4bit() -> Outcome {
    let ncpus = per_cpu_init::cpu_count() as u32;
    let bits = asid::bits();
    // The cap holds wherever 15 ASIDs exceed the CPUs; past that the port
    // runs without ASIDs, and this test checks that fallback instead.
    if ncpus < 15 && bits != Some(vibeos::mm::asid::TEST_CAP_BITS) {
        return crate::fail_fmt!("asid width {bits:?}: the boot did not select {CAP}");
    }
    let gen0 = asid::generation();
    let aliases0 = asid::aliases();
    // Relaxed: set before any spawner starts, which `spawn_thread_on` orders; pairs with nothing.
    CLAIMED.store(0, Ordering::Relaxed);
    // Relaxed: as LEFT; pairs with nothing.
    PASSED.store(0, Ordering::Relaxed);
    // Relaxed: as LEFT; pairs with nothing.
    FIRST_BAD.store(u64::MAX, Ordering::Relaxed);
    // Relaxed: as LEFT; pairs with nothing.
    SPAWNERS_DONE.store(0, Ordering::Relaxed);
    let mask = per_cpu_init::online_mask();
    let mut ids = [ThreadId::NONE; 64];
    let mut started = 0u32;
    for cpu in 0..64u32 {
        if mask & (1u64 << cpu) != 0 {
            ids[cpu as usize] = spawn_thread_on("asid-spawner", spawner, cpu).id();
            started += 1;
        }
    }
    // Acquire: pairs with the Release add in `spawner`.
    let done = sleep_until(|| SPAWNERS_DONE.load(Ordering::Acquire) == started, 170_000);
    let joined = sleep_until(
        || {
            ids.iter()
                .all(|&id| id == ThreadId::NONE || thread_init::exited(id))
        },
        2_000,
    );
    if !done {
        // Relaxed: progress for the failure line; pairs with nothing.
        let claimed = CLAIMED.load(Ordering::Relaxed).min(PROCS);
        return crate::fail_fmt!("spawners not done, {claimed} of {PROCS} processes started");
    }
    // Acquire: pairs with the AcqRel compare-exchange in `note_bad`.
    match FIRST_BAD.load(Ordering::Acquire) {
        u64::MAX => {}
        v if v == u64::MAX - 1 => return Outcome::Fail("spawn failed"),
        st => return crate::fail_fmt!("a process read another's pattern: status {st:#x}"),
    }
    // Relaxed: ordered after every spawner's adds by `SPAWNERS_DONE`; pairs with nothing.
    let passed = PASSED.load(Ordering::Relaxed);
    if passed != PROCS {
        return crate::fail_fmt!("{passed} of {PROCS} processes passed");
    }
    let aliases = asid::aliases().saturating_sub(aliases0);
    if aliases != 0 {
        return crate::fail_fmt!("{aliases} loads gave an ASID a second space before a flush");
    }
    let rollovers = asid::generation().saturating_sub(gen0) >> bits.unwrap_or(0);
    crate::ktest_info!("asid bits={bits:?} cpus={ncpus} rollovers={rollovers}");
    // 15 usable, at most `ncpus` reserved: each generation serves at most 15
    // new spaces, so 1,000 spaces need more than 60 rollovers.
    if bits.is_some() && rollovers < 60 {
        return crate::fail_fmt!("only {rollovers} rollovers");
    }
    if !joined {
        return Outcome::Fail("a spawner did not exit");
    }
    Outcome::Ok
}

static TTBR0_ID_LIVE: AtomicU64 = AtomicU64::new(0);
static TTBR0_SEEN: AtomicU64 = AtomicU64::new(0);

fn ttbr0_on_ap(_: *mut ()) {
    let id = crate::per_cpu_init::current().cpu_id;
    if id >= 64 {
        return;
    }
    let empty = crate::paging_init::empty_user_root();
    let live =
        crate::arch::aarch64::cpu::read_ttbr0() & vibeos::arch::aarch64::paging::DESC_ADDR_MASK;
    // A user thread's root may map any low address; test only the empty one.
    if live == empty
        && crate::arch::aarch64::secondary::entry_pa()
            .is_some_and(crate::arch::aarch64::cpu::stage1_read_translates)
    {
        // Relaxed: pairs with nothing; the Release `fetch_or` on `TTBR0_SEEN` below publishes it.
        TTBR0_ID_LIVE.fetch_or(1u64 << id, Ordering::Relaxed);
    }
    // Release: pairs with the Acquire load in `test_ap_ttbr0_empty`.
    TTBR0_SEEN.fetch_or(1u64 << id, Ordering::Release);
}

/// Every AP left the secondary stub with TTBR0 at the empty user root, and
/// the stub's identity address faults there, on arrival and now (ROADMAP
/// §11.4).
pub(crate) fn test_ap_ttbr0_empty() -> Outcome {
    let empty = crate::paging_init::empty_user_root();
    if empty == 0 {
        return Outcome::Fail("no empty TTBR0");
    }
    let Some(stub) = crate::arch::aarch64::secondary::entry_pa() else {
        return Outcome::Fail("no stub pa");
    };
    let aps = super::ktest::ap_mask();
    if aps == 0 {
        return Outcome::Skip("no AP");
    }
    let mut i = 1u32;
    while i < 64 {
        if aps & (1u64 << i) != 0 {
            let Some((ttbr0, id_live)) = crate::arch::aarch64::secondary::ap_entry_ttbr0(i) else {
                return crate::fail_fmt!("cpu {i} recorded no arrival ttbr0");
            };
            let root = ttbr0 & vibeos::arch::aarch64::paging::DESC_ADDR_MASK;
            if root != empty {
                return crate::fail_fmt!("cpu {i} arrived on ttbr0 {root:#x} != empty {empty:#x}");
            }
            if id_live {
                return crate::fail_fmt!("cpu {i} arrived with stub pa {stub:#x} mapped");
            }
        }
        i += 1;
    }
    TTBR0_ID_LIVE.store(0, Ordering::SeqCst);
    TTBR0_SEEN.store(0, Ordering::SeqCst);
    crate::ipi_init::call_mask(aps, ttbr0_on_ap, core::ptr::null_mut(), true);
    // Acquire: pairs with the Release `fetch_or` in `ttbr0_on_ap`.
    if TTBR0_SEEN.load(Ordering::Acquire) != aps {
        return Outcome::Fail("ap did not probe ttbr0");
    }
    // Relaxed: pairs with nothing; the Acquire load of `TTBR0_SEEN` above orders it.
    let bad = TTBR0_ID_LIVE.load(Ordering::Relaxed);
    if bad != 0 {
        return crate::fail_fmt!("stub pa {stub:#x} translates on aps {bad:#x}");
    }
    Outcome::Ok
}
