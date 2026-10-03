//! Host tests of the buddy's top-order runs (ROADMAP §10.2): whole free
//! `MAX_ORDER` blocks that `insert_region` keeps as runs, unlinked, until
//! an allocation takes them.

use super::testing::TEST_PHYS_BASE;
use super::*;

use std::panic;
use std::vec;
use std::vec::Vec;

const TOP: u64 = PAGE_SIZE << MAX_ORDER;
/// What the run tests plant in the first word of each top block before
/// `insert_region`, so a node written there shows.
const PLANTED: u64 = 0x5EED_0F0F_5EED_0F0F;

/// `frames` frames from `TEST_PHYS_BASE` in zeroed host memory, which
/// Miri allocates without running a fill loop, with [`PLANTED`] in the
/// first word of each whole top block; then `insert_region` over them.
struct RunPool {
    mem: Vec<u64>,
    buddy: Buddy,
}

impl RunPool {
    fn new(frames: usize) -> Self {
        let mut mem = vec![0u64; frames * PAGE_SIZE as usize / 8];
        let words_per_top = (TOP / 8) as usize;
        let mut i = 0usize;
        while i + words_per_top <= mem.len() {
            mem[i] = PLANTED;
            i += words_per_top;
        }
        let hhdm = (mem.as_ptr() as u64).wrapping_sub(TEST_PHYS_BASE);
        let mut buddy = Buddy::new(hhdm);
        let end = TEST_PHYS_BASE + frames as u64 * PAGE_SIZE;
        // SAFETY: `insert_region`'s contract; `mem` is `frames` pages
        // of host memory this pool owns, reached at `hhdm`, and on no
        // list yet (invariant I46, established here).
        unsafe { buddy.insert_region(TEST_PHYS_BASE, end) };
        Self { mem, buddy }
    }

    /// The first word of frame `phys`.
    fn word(&self, phys: u64) -> u64 {
        self.mem[((phys - TEST_PHYS_BASE) / 8) as usize]
    }
}

#[test]
fn top_blocks_stay_unlinked_until_taken() {
    let mut p = RunPool::new(2 * 1024 + 5);
    // Two whole top blocks become one run: no node in either.
    assert_eq!(p.buddy.nruns, 1);
    assert_eq!(p.buddy.runs[0], (TEST_PHYS_BASE, TEST_PHYS_BASE + 2 * TOP));
    assert_eq!(p.word(TEST_PHYS_BASE), PLANTED);
    assert_eq!(p.word(TEST_PHYS_BASE + TOP), PLANTED);
    // The 5-frame tail is linked, as before: its order-2 block holds
    // the order-2 list's only node.
    assert_eq!(p.buddy.heads[2], TEST_PHYS_BASE + 2 * TOP);
    let s0 = p.buddy.stats();
    assert_eq!(s0.total_frames, 2053);
    assert_eq!(s0.free_frames, 2053);
    assert_eq!(s0.largest_free_order, Some(MAX_ORDER as u8));
    // The highest block first, and the lower one stays untouched.
    let a = p.buddy.alloc(MAX_ORDER as u8).unwrap();
    assert_eq!(a.base(), TEST_PHYS_BASE + TOP);
    assert_eq!(p.word(TEST_PHYS_BASE), PLANTED);
    // No list from order 3 up: the run's last block splits.
    let b = p.buddy.alloc(3).unwrap();
    assert_eq!(b.base(), TEST_PHYS_BASE);
    assert_eq!(p.buddy.nruns, 0);
    assert_eq!(p.buddy.stats().free_frames, 2053 - 1024 - 8);
    p.buddy.free(b);
    p.buddy.free(a);
    // Whole again, and now linked.
    assert_eq!(p.buddy.stats(), s0);
    assert_eq!(p.buddy.counts[MAX_ORDER], 2);
    assert_ne!(p.buddy.heads[MAX_ORDER], NULL);
    assert_ne!(p.word(TEST_PHYS_BASE), PLANTED);
}

#[test]
fn free_inside_a_run_is_a_double_free() {
    let mut p = RunPool::new(1024);
    assert_eq!(p.buddy.nruns, 1);
    let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        // SAFETY: forges a token on purpose; `free` refuses a frame of
        // a run with a panic before touching the lists, established
        // here.
        p.buddy
            .free(unsafe { Frames::from_entry(TEST_PHYS_BASE + PAGE_SIZE, 0) });
    }));
    assert!(res.is_err(), "a frame of a run is free already");
}

#[test]
fn alloc_constrained_takes_a_fitting_run_block() {
    let mut p = RunPool::new(2 * 1024);
    let s0 = p.buddy.stats();
    // Only the lower block's low frame ends at or below the limit.
    let limit = TEST_PHYS_BASE + TOP;
    let f = p.buddy.alloc_constrained(0, limit).unwrap();
    assert_eq!(f.base(), TEST_PHYS_BASE);
    assert_eq!(
        p.buddy.runs[0],
        (TEST_PHYS_BASE + TOP, TEST_PHYS_BASE + 2 * TOP)
    );
    // The next comes from the split halves, not the run.
    let g = p.buddy.alloc_constrained(0, limit).unwrap();
    assert!(g.base() + PAGE_SIZE <= limit);
    assert_eq!(p.buddy.nruns, 1);
    // A whole top block under the pool's end: the run's last.
    let h = p
        .buddy
        .alloc_constrained(MAX_ORDER as u8, TEST_PHYS_BASE + 2 * TOP)
        .unwrap();
    assert_eq!(h.base(), TEST_PHYS_BASE + TOP);
    assert_eq!(p.buddy.nruns, 0);
    assert!(p.buddy.alloc_constrained(0, TEST_PHYS_BASE).is_none());
    p.buddy.free(f);
    p.buddy.free(g);
    p.buddy.free(h);
    assert_eq!(p.buddy.stats(), s0);
}

#[test]
fn full_run_table_links_the_rest() {
    let mut mem = vec![0u64; (TOP / 8) as usize];
    mem[0] = PLANTED;
    let hhdm = (mem.as_ptr() as u64).wrapping_sub(TEST_PHYS_BASE);
    let mut b = Buddy::new(hhdm);
    // Fill every slot with runs far from the pool, which no call here
    // takes a block from.
    let far = 1u64 << 40;
    for i in 0..TOP_RUNS as u64 {
        assert!(b.add_run(far + 2 * i * TOP, far + (2 * i + 1) * TOP));
    }
    assert!(!b.add_run(far + 64 * TOP, far + 65 * TOP));
    // A range that continues the last run extends it.
    let last = far + (2 * TOP_RUNS as u64 - 1) * TOP;
    assert!(b.add_run(last, last + TOP));
    // SAFETY: `insert_region`'s contract; `mem` is one top block of
    // host memory this test owns, reached at `hhdm`, and on no list
    // yet (invariant I46, established here).
    unsafe { b.insert_region(TEST_PHYS_BASE, TEST_PHYS_BASE + TOP) };
    assert_eq!(b.heads[MAX_ORDER], TEST_PHYS_BASE);
    assert_ne!(mem[0], PLANTED);
    // A linked block goes before the runs.
    let f = b.alloc(MAX_ORDER as u8).unwrap();
    assert_eq!(f.base(), TEST_PHYS_BASE);
    b.free(f);
    assert_eq!(b.heads[MAX_ORDER], TEST_PHYS_BASE);
}
