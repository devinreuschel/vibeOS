//! Kani proofs of the buddy allocator (ROADMAP §10.8): `make models` runs
//! them. Each harness's doc comment states its bound.

use super::*;
use crate::atomic::statics::AtomicPtr;

/// `order_for` never panics (Kani's overflow checks), and a `Some`
/// order is at most `MAX_ORDER`, its block covers `bytes`, and the
/// block, aligned to its own size, is `align`-aligned.
///
/// Bound: every pair of 64-bit `(bytes, align)`; the function has no
/// loop.
#[kani::proof]
fn order_for_covers_and_aligns() {
    let bytes: u64 = kani::any();
    let align: u64 = kani::any();
    let got = Buddy::order_for(bytes, align);
    kani::cover!(got.is_none(), "refused");
    kani::cover!(got == Some(0), "order 0");
    kani::cover!(got == Some(MAX_ORDER as u8), "largest order");
    if let Some(order) = got {
        assert!(order as usize <= MAX_ORDER);
        let block = PAGE_SIZE << order;
        assert!(block >= bytes);
        assert!(align == 0 || block % align == 0);
    }
}

/// Frames in the arena: one order-4 block.
const ARENA_FRAMES: usize = 16;
/// The arena's physical base: 64 KiB aligned, so the 16 frames form one
/// order-4 block, and not frame 0, which never enters the buddy.
const ARENA_BASE: u64 = 0x10_0000;
const ARENA_END: u64 = ARENA_BASE + ARENA_FRAMES as u64 * PAGE_SIZE;
/// Highest order the harnesses allocate.
const TOP: u8 = 4;
/// Allocations in `buddy_16_frames_five_allocs`, and so its live blocks.
const ALLOCS: usize = 5;

/// The arena after `insert_region`: one order-4 block.
const ONE_BLOCK: PmmStats = PmmStats {
    total_frames: ARENA_FRAMES,
    free_frames: ARENA_FRAMES,
    largest_free_order: Some(TOP),
};

/// The real `Buddy` on a 16-frame arena: every sequence of up to five
/// `alloc` calls, each of any order 0 to 4, keeps the live blocks inside
/// the arena, aligned to their size and pairwise disjoint, the free count
/// exact, and no free block at orders 0 to 3 beside its buddy on the same
/// list; an allocation is refused only when no free block is large
/// enough. The checks run after setup and after every call, so every
/// shorter sequence is covered too. Freeing is
/// `buddy_16_frames_alloc_then_free`'s: a symbolic free walks up to 21
/// free lists, and sequences that mix frees ran CBMC out of memory.
///
/// Bound: 16 frames, orders 0 to 4, at most 5 allocations (5 verify in
/// 15 minutes; 6 did not finish in 50); loops unwound
/// 18 times (no free list holds more than 16 nodes, and the order loops
/// run `MAX_ORDER + 1 = 11` times).
///
/// Kani's view of memory: [`node_ptr_in_frame`] stands in for
/// `Buddy::node_ptr` and asserts that every node the buddy touches is
/// the first bytes of a frame of the arena; every other line of `Buddy`
/// is the kernel's.
#[kani::proof]
#[kani::unwind(18)]
#[kani::stub(Buddy::node_ptr, node_ptr_in_frame)]
fn buddy_16_frames_five_allocs() {
    let mut nodes = [const { FreeNode { next: 0, prev: 0 } }; ARENA_FRAMES];
    // `node_ptr_in_frame` reads it only while `nodes` lives. One thread,
    // so Relaxed.
    NODES.store(&raw mut nodes, Ordering::Relaxed);
    // The stub ignores the offset; 0 names no real mapping.
    let mut b = Buddy::new(0);
    // SAFETY: `insert_region`'s contract; the arena's 16 frames reach
    // their nodes in `nodes`, writable memory this harness owns, on no
    // list yet, which only the buddy writes until the harness ends
    // (invariant I224, established here).
    unsafe { b.insert_region(ARENA_BASE, ARENA_END) };
    assert_eq!(b.stats(), ONE_BLOCK);
    let mut live = Live::<ALLOCS>::new();
    check(&b, &live);
    let mut n = 0;
    while n < ALLOCS {
        let order: u8 = kani::any();
        kani::assume(order <= TOP);
        match b.alloc(order) {
            Some(f) => {
                kani::cover!(order == 0, "alloc order 0");
                kani::cover!(order == 1, "alloc order 1");
                kani::cover!(order == 2, "alloc order 2");
                kani::cover!(order == 3, "alloc order 3");
                kani::cover!(order == 4, "alloc order 4");
                live.keep(n, f);
            }
            None => {
                let st = b.stats();
                assert!(st.largest_free_order.is_none_or(|l| l < order));
                kani::cover!(st.free_frames == 0, "alloc refused, arena full");
            }
        }
        check(&b, &live);
        n += 1;
    }
    // The blocks stay allocated: freeing is the other proof's.
}

/// The real `Buddy` on a 16-frame arena: an allocation of any order 0 to
/// 4 succeeds on the empty arena, and freeing it merges every split back,
/// leaving one order-4 block and `stats()` as before; the checks run
/// after each call.
///
/// Bound: 16 frames, orders 0 to 4, one allocation and its free; loops
/// unwound 18 times, as `buddy_16_frames_five_allocs`.
#[kani::proof]
#[kani::unwind(18)]
#[kani::stub(Buddy::node_ptr, node_ptr_in_frame)]
fn buddy_16_frames_alloc_then_free() {
    let mut nodes = [const { FreeNode { next: 0, prev: 0 } }; ARENA_FRAMES];
    // `node_ptr_in_frame` reads it only while `nodes` lives. One thread,
    // so Relaxed.
    NODES.store(&raw mut nodes, Ordering::Relaxed);
    // The stub ignores the offset; 0 names no real mapping.
    let mut b = Buddy::new(0);
    // SAFETY: `insert_region`'s contract; the arena's 16 frames reach
    // their nodes in `nodes`, writable memory this harness owns, on no
    // list yet, which only the buddy writes until the harness ends
    // (invariant I224, established here).
    unsafe { b.insert_region(ARENA_BASE, ARENA_END) };
    assert_eq!(b.stats(), ONE_BLOCK);
    let order: u8 = kani::any();
    kani::assume(order <= TOP);
    let mut live = Live::<1>::new();
    let f = b.alloc(order);
    assert!(f.is_some());
    if let Some(f) = f {
        live.keep(0, f);
    }
    check(&b, &live);
    b.free(live.take(0));
    check(&b, &live);
    assert_eq!(b.stats(), ONE_BLOCK);
    kani::cover!(order < TOP, "free merges back to order 4");
}

/// The free-list node of each arena frame, in frame order.
static NODES: AtomicPtr<[FreeNode; ARENA_FRAMES]> = AtomicPtr::new(core::ptr::null_mut());

/// `Buddy::node_ptr` for the verifier: frame `i` of the arena keeps its
/// node in `NODES[i]`. A free block's node is the only memory the buddy
/// reads or writes, so one slot per frame stands for the frame, and the
/// assert checks that every address the buddy asks for is a frame of
/// the arena. With the kernel's `phys + hhdm` integer-to-pointer cast
/// over a 64 KiB arena, CBMC cannot tell which object the pointer names,
/// and even a one-call run exhausts a 15 GiB host.
fn node_ptr_in_frame(_b: &Buddy, phys: u64) -> *mut FreeNode {
    let off = phys.wrapping_sub(ARENA_BASE);
    assert!(off < ARENA_END - ARENA_BASE && off % PAGE_SIZE == 0);
    let i = (off / PAGE_SIZE) as usize;
    let nodes = NODES.load(Ordering::Relaxed);
    // SAFETY: `nodes` points at the harness's `nodes`, live until the
    // harness returns, and the assert above makes `i < 16`; established
    // here and at `mm::pmm::kani_proofs::buddy_16_frames_five_allocs`.
    unsafe { &raw mut (*nodes)[i] }
}

/// Live blocks as plain words: an `[Option<Frames>; N]` reached Kani
/// 0.68 uninitialized (a `None` element read as a block at 2^49). A
/// block's token moves in by `Frames::into_entry` and back out by
/// `Frames::from_entry`, so no token is dropped (C-FRAMES).
struct Live<const N: usize> {
    used: [bool; N],
    base: [u64; N],
    order: [u8; N],
}

impl<const N: usize> Live<N> {
    fn new() -> Self {
        Self {
            used: [false; N],
            base: [0; N],
            order: [0; N],
        }
    }

    fn keep(&mut self, i: usize, f: Frames) {
        self.order[i] = f.order();
        self.base[i] = f.into_entry();
        self.used[i] = true;
    }

    fn take(&mut self, i: usize) -> Frames {
        assert!(self.used[i]);
        self.used[i] = false;
        // SAFETY: `Frames::from_entry`'s contract; slot `i` held the
        // `into_entry` of a token of this order, and clearing it above
        // leaves nothing else naming the block; established here.
        unsafe { Frames::from_entry(self.base[i], self.order[i]) }
    }
}

/// The invariants the buddy proofs assert after each call.
/// Plain index loops: CBMC unwinds iterator adapters slowly.
fn check<const N: usize>(b: &Buddy, live: &Live<N>) {
    let mut used = 0usize;
    let mut i = 0;
    while i < N {
        if live.used[i] {
            let (base, size) = (live.base[i], PAGE_SIZE << live.order[i]);
            assert!(live.order[i] <= TOP);
            assert!(base >= ARENA_BASE && base + size <= ARENA_END);
            // A mask, not `%`: `size` is a power of two, and a divider
            // on a symbolic operand swamps CBMC.
            assert!(base & (size - 1) == 0);
            let mut j = i + 1;
            while j < N {
                if live.used[j] {
                    let (gb, gs) = (live.base[j], PAGE_SIZE << live.order[j]);
                    assert!(base + size <= gb || gb + gs <= base);
                }
                j += 1;
            }
            used += 1 << live.order[i];
        }
        i += 1;
    }
    let st = b.stats();
    assert!(st.total_frames == ARENA_FRAMES);
    assert!(st.free_frames == ARENA_FRAMES - used);
    // Merged: mark each order-k free block's first frame, then no
    // aligned pair of order-k blocks (a block and its buddy) is marked
    // twice. One walk per list keeps the check linear.
    let mut k = 0;
    while k < TOP {
        let mut on_list = [false; ARENA_FRAMES];
        let mut cur = b.heads[k as usize];
        while cur != NULL {
            assert!(cur >= ARENA_BASE && cur < ARENA_END);
            on_list[((cur - ARENA_BASE) / PAGE_SIZE) as usize] = true;
            // SAFETY: `cur` is a node on the order-`k` free list, in a
            // free frame of the arena only the buddy writes (invariant
            // I224, established at `mm::pmm::Buddy::insert_region`).
            cur = unsafe { (*b.node_ptr(cur)).next };
        }
        let step = 1usize << k;
        let mut f = 0;
        while f < ARENA_FRAMES {
            assert!(!(on_list[f] && on_list[f + step]));
            f += 2 * step;
        }
        k += 1;
    }
}
