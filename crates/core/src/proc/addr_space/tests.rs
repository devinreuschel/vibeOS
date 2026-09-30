use super::*;
use crate::arch::stub::Arch;
use crate::paging::{UserFreeStats, physmap_flags};
use crate::pmm::testing::Pool;

/// Frames the pool has handed out and not had back.
fn used(pool: &Pool) -> usize {
    let s = pool.buddy.stats();
    s.total_frames - s.free_frames
}

/// The root's token moves into the mapper, which is never torn down.
fn kernel_mapper(pool: &mut Pool) -> Mapper<Arch> {
    let root = PhysAddr(pool.alloc_frame().unwrap().into_entry());
    // SAFETY: `root` is an owned frame fresh from `pool`, zeroed on the next line before any walk,
    // and `pool.hhdm()` maps every pool frame writable; established here.
    let mapper = unsafe { Mapper::new(root, pool.hhdm()) };
    // SAFETY: `root` is an owned pool frame reachable through `pool.hhdm()`; established here.
    unsafe { mapper.zero_frame(root) };
    mapper
}

#[test]
fn top_page_is_not_mappable() {
    let mut pool = Pool::new(64);
    let kernel = kernel_mapper(&mut pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut aspace =
        unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    assert_eq!(
        // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
        // anything, and `pool` hands out owned frames; established here.
        unsafe { aspace.map_anon(USER_MAP_END, PAGE_SIZE_4K, UserPerms::RW, &mut pool) },
        Err(AsError::KernelRange)
    );
    let below = USER_MAP_END - PAGE_SIZE_4K;
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    assert!(unsafe { aspace.map_anon(below, PAGE_SIZE_4K, UserPerms::RW, &mut pool) }.is_ok());
    assert!(aspace.check_user_range(below, PAGE_SIZE_4K).is_ok());
    assert!(aspace.check_user_range(USER_MAP_END, 1).is_err());
    assert!(aspace.check_user_range(below, PAGE_SIZE_4K + 1).is_err());
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { aspace.teardown_pool(&mut pool) };
}

#[test]
fn null_guard_and_kernel_rejected() {
    let mut pool = Pool::new(64);
    let kernel = kernel_mapper(&mut pool);
    let before = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut aspace =
        unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    assert_eq!(
        // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
        // anything, and `pool` hands out owned frames; established here.
        unsafe { aspace.map_anon(0, PAGE_SIZE_4K, UserPerms::RW, &mut pool) },
        Err(AsError::NullGuard)
    );
    assert_eq!(
        // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
        // anything, and `pool` hands out owned frames; established here.
        unsafe {
            aspace.map_anon(
                crate::paging::USER_END,
                PAGE_SIZE_4K,
                UserPerms::RW,
                &mut pool,
            )
        },
        Err(AsError::KernelRange)
    );
    assert_eq!(
        // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
        // anything, and `pool` hands out owned frames; established here.
        unsafe {
            aspace.map_anon(
                0xFFFF_8000_0000_0000,
                PAGE_SIZE_4K,
                UserPerms::RW,
                &mut pool,
            )
        },
        Err(AsError::KernelRange)
    );
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { aspace.teardown_pool(&mut pool) };
    assert_eq!(used(&pool), before);
}

#[test]
fn map_unmap_teardown_balances_frames() {
    let mut pool = Pool::new(128);
    let mut kernel = kernel_mapper(&mut pool);
    let kva = VirtAddr(0xFFFF_C000_0010_0000);
    // SAFETY: this kernel mapper is never loaded in a CR3, so its kernel-half leaf reaches no live
    // memory, and its tables come from `pool`; established here.
    unsafe {
        kernel
            .map_page(
                kva,
                PhysAddr(0x0080_0000),
                physmap_flags(),
                PageSize::Size4K,
                MapMode::Fresh,
                &mut pool,
            )
            .unwrap();
    }
    let before = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut aspace =
        unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    assert_eq!(
        aspace.mapper().pml4_entry(Arch::KERNEL_ROOT_FIRST),
        kernel.pml4_entry(Arch::KERNEL_ROOT_FIRST)
    );
    let user_va = 0x0000_0000_0040_0000u64;
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe {
        aspace
            .map_anon(user_va, PAGE_SIZE_4K * 2, UserPerms::RW, &mut pool)
            .unwrap();
    }
    assert_eq!(aspace.user_frames(), 2);
    assert!(aspace.pt_frames() >= 2);
    assert!(aspace.check_user_range(user_va, 16).is_ok());
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe {
        aspace
            .unmap_free(user_va, PAGE_SIZE_4K * 2, &mut pool, &mut |_| {})
            .unwrap();
    }
    assert_eq!(aspace.user_frames(), 0);
    assert_eq!(
        aspace.check_user_range(user_va, 16),
        Err(UserMemError::Unmapped)
    );
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    let st = unsafe { aspace.teardown_pool(&mut pool) };
    assert_eq!(st.user_frames, 0);
    assert!(st.pt_frames >= 1);
    assert_eq!(used(&pool), before);
    assert!(kernel.translate(kva).is_some());
}

#[test]
fn user_ptr_helpers() {
    let mut pool = Pool::new(64);
    let kernel = kernel_mapper(&mut pool);
    let before = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut aspace =
        unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    let va = 0x0000_0000_0040_0000u64;
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe {
        aspace
            .map_anon(va, PAGE_SIZE_4K, UserPerms::RW, &mut pool)
            .unwrap();
    }
    assert_eq!(aspace.check_user_range(0, 8), Err(UserMemError::NullGuard));
    assert_eq!(
        aspace.check_user_range(0xFFFF_8000_0000_1000, 8),
        Err(UserMemError::Kernel)
    );
    assert_eq!(
        aspace.check_user_range(u64::MAX, 2),
        Err(UserMemError::Overflow)
    );
    assert_eq!(
        aspace.check_user_range(0x0000_8000_0000_0000, 8),
        Err(UserMemError::NonCanonical)
    );
    assert_eq!(
        aspace.check_user_range(va + PAGE_SIZE_4K, 8),
        Err(UserMemError::Unmapped)
    );
    assert!(aspace.check_user_range(va, 8).is_ok());
    assert!(aspace.check_user_range(va, 0).is_ok());
    aspace.write_bytes(va, b"abcd").unwrap();
    let mut got = [0u8; 4];
    aspace.read_bytes(va, &mut got).unwrap();
    assert_eq!(&got, b"abcd");
    aspace.zero_bytes(va, 2).unwrap();
    aspace.read_bytes(va, &mut got).unwrap();
    assert_eq!(&got, b"\0\0cd");
    assert_eq!(aspace.write_bytes(0, b"x"), Err(UserMemError::NullGuard));
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { aspace.teardown_pool(&mut pool) };
    assert_eq!(used(&pool), before);
    let _ = UserFreeStats {
        leaves: 0,
        tables: 0,
    };
}

#[test]
fn clone_anon_copies_bytes_not_frames() {
    let mut pool = Pool::new(128);
    let kernel = kernel_mapper(&mut pool);
    let before = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut src =
        unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    let va = 0x0000_0000_0040_0000u64;
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe {
        src.map_anon(va, PAGE_SIZE_4K, UserPerms::RW, &mut pool)
            .unwrap();
    }
    src.write_bytes(va, b"fork-me").unwrap();
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let dst = unsafe { src.clone_anon(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    let mut got = [0u8; 7];
    dst.read_bytes(va, &mut got).unwrap();
    assert_eq!(&got, b"fork-me");
    src.write_bytes(va, b"parent!").unwrap();
    dst.read_bytes(va, &mut got).unwrap();
    assert_eq!(&got, b"fork-me");
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe {
        let mut src = src;
        src.teardown_pool(&mut pool);
        let mut dst = dst;
        dst.teardown_pool(&mut pool);
    }
    assert_eq!(used(&pool), before);
}

#[test]
fn kernel_half_not_owned() {
    let mut pool = Pool::new(64);
    let mut kernel = kernel_mapper(&mut pool);
    // SAFETY: this kernel mapper is never loaded in a CR3, so its kernel-half leaf reaches no live
    // memory, and its tables come from `pool`; established here.
    unsafe {
        kernel
            .map_page(
                VirtAddr(0xFFFF_8000_0020_0000),
                PhysAddr(0x0040_0000),
                physmap_flags(),
                PageSize::Size2M,
                MapMode::Fresh,
                &mut pool,
            )
            .unwrap();
    }
    let before = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut aspace =
        unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    assert_eq!(
        Arch::entry_phys(aspace.mapper().pml4_entry(256)),
        Arch::entry_phys(kernel.pml4_entry(256))
    );
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { aspace.teardown_pool(&mut pool) };
    // The shared kernel-half tables stay allocated; only the root went.
    assert_eq!(used(&pool), before);
    assert!(kernel.translate(VirtAddr(0xFFFF_8000_0020_0000)).is_some());
}

#[test]
fn user_mem_error_is_efault() {
    for e in [
        UserMemError::NonCanonical,
        UserMemError::Kernel,
        UserMemError::Overflow,
        UserMemError::Unmapped,
        UserMemError::NullGuard,
    ] {
        match e {
            UserMemError::NonCanonical
            | UserMemError::Kernel
            | UserMemError::Overflow
            | UserMemError::Unmapped
            | UserMemError::NullGuard => assert_eq!(crate::kerror::KError::from(e).errno(), 14),
        }
    }
}

#[test]
fn map_anon_rolls_back_on_leaf_oom() {
    let mut pool = Pool::new(16);
    let kernel = kernel_mapper(&mut pool);
    let baseline = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut aspace =
        unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    let after_new = used(&pool);
    let va = 0x0000_0000_0040_0000u64;
    assert_eq!(
        // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
        // anything, and `pool` hands out owned frames; established here.
        unsafe { aspace.map_anon(va, 64 * PAGE_SIZE_4K, UserPerms::RW, &mut pool) },
        Err(AsError::OutOfFrames)
    );
    assert_eq!(aspace.user_frames(), 0);
    assert_eq!(aspace.regions().count(), 0);
    assert_eq!(
        aspace.check_user_range(va, PAGE_SIZE_4K),
        Err(UserMemError::Unmapped)
    );
    assert_eq!(
        aspace.check_user_range(va + 8 * PAGE_SIZE_4K, PAGE_SIZE_4K),
        Err(UserMemError::Unmapped)
    );
    assert_eq!(used(&pool), after_new + aspace.pt_frames() - 1);
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe {
        aspace
            .map_anon(va, 2 * PAGE_SIZE_4K, UserPerms::RW, &mut pool)
            .unwrap();
    }
    assert_eq!(aspace.user_frames(), 2);
    assert_eq!(aspace.regions().count(), 1);
    assert_eq!(used(&pool), after_new + aspace.pt_frames() - 1 + 2);
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { aspace.teardown_pool(&mut pool) };
    assert_eq!(used(&pool), baseline);
}

const P: u64 = PAGE_SIZE_4K;
const BASE: u64 = 0x0000_0000_0040_0000;

fn nop(_: u64) {}

#[test]
fn addr_space_munmap_splits_region() {
    let mut pool = Pool::new(128);
    let kernel = kernel_mapper(&mut pool);
    let before = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut a = unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe { a.map_anon(BASE, 3 * P, UserPerms::RW, &mut pool).unwrap() };
    a.write_bytes(BASE, b"one").unwrap();
    a.write_bytes(BASE + 2 * P, b"three").unwrap();
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe { a.unmap_free(BASE + P, P, &mut pool, &mut nop).unwrap() };
    let mut rs: Vec<Region> = a.regions().collect();
    rs.sort_by_key(|r| r.start);
    assert_eq!(rs.len(), 2);
    assert_eq!((rs[0].start, rs[0].len), (BASE, P));
    assert_eq!((rs[1].start, rs[1].len), (BASE + 2 * P, P));
    assert_eq!(a.user_frames(), 2);
    assert_eq!(a.check_user_range(BASE + P, 1), Err(UserMemError::Unmapped));
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut c = unsafe { a.clone_anon(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    let mut got = [0u8; 5];
    c.read_bytes(BASE, &mut got[..3]).unwrap();
    assert_eq!(&got[..3], b"one");
    c.read_bytes(BASE + 2 * P, &mut got).unwrap();
    assert_eq!(&got, b"three");
    assert_eq!(c.user_frames(), 2);
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe {
        c.teardown_pool(&mut pool);
        a.teardown_pool(&mut pool);
    }
    assert_eq!(used(&pool), before);
}

#[test]
fn munmap_trims_spans_and_holes() {
    let mut pool = Pool::new(128);
    let kernel = kernel_mapper(&mut pool);
    let before = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut a = unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    let r2 = BASE + 8 * P;
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe {
        a.map_anon(BASE, 4 * P, UserPerms::RW, &mut pool).unwrap();
        a.map_anon(r2, 4 * P, UserPerms::RW, &mut pool).unwrap();
    }
    let base_used = used(&pool) - a.user_frames();
    // Head of the first region.
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe { a.unmap_free(BASE, P, &mut pool, &mut nop).unwrap() };
    // Tail of the second.
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe { a.unmap_free(r2 + 3 * P, P, &mut pool, &mut nop).unwrap() };
    assert_eq!(a.user_frames(), 6);
    // A span over the first's tail, the hole, and the second's head.
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe {
        a.unmap_free(BASE + 2 * P, 8 * P, &mut pool, &mut nop)
            .unwrap()
    };
    let mut rs: Vec<(u64, u64)> = a.regions().map(|r| (r.start, r.len)).collect();
    rs.sort();
    assert_eq!(rs, [(BASE + P, P), (r2 + 2 * P, P)]);
    assert_eq!(a.user_frames(), 2);
    // A hole, an empty range, and a range below the null guard.
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe {
        a.unmap_free(BASE + 4 * P, 4 * P, &mut pool, &mut nop)
            .unwrap();
        a.unmap_free(BASE, 0, &mut pool, &mut nop).unwrap();
        a.unmap_free(0, P, &mut pool, &mut nop).unwrap();
    }
    assert_eq!(a.user_frames(), 2);
    assert_eq!(used(&pool), base_used + a.user_frames());
    assert_eq!(
        // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
        // established here.
        unsafe { a.unmap_free(BASE + 1, P, &mut pool, &mut nop) },
        Err(AsError::Misaligned)
    );
    assert_eq!(
        // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
        // established here.
        unsafe { a.unmap_free(USER_MAP_END, P, &mut pool, &mut nop) },
        Err(AsError::KernelRange)
    );
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { a.teardown_pool(&mut pool) };
    assert_eq!(used(&pool), before);
}

#[test]
fn munmap_split_needs_slot() {
    let mut pool = Pool::new(MAX_REGIONS + 128);
    let kernel = kernel_mapper(&mut pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut a = unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe { a.map_anon(BASE, 3 * P, UserPerms::RW, &mut pool).unwrap() };
    let mut va = BASE + 4 * P;
    while a.regions().count() < MAX_REGIONS {
        // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
        // anything, and `pool` hands out owned frames; established here.
        unsafe { a.map_anon(va, P, UserPerms::RW, &mut pool).unwrap() };
        va += 2 * P;
    }
    let frames = a.user_frames();
    let used_before = used(&pool);
    assert_eq!(
        // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
        // established here.
        unsafe { a.unmap_free(BASE + P, P, &mut pool, &mut nop) },
        Err(AsError::NoRegionSlot)
    );
    assert_eq!(a.user_frames(), frames);
    assert_eq!(used(&pool), used_before);
    assert!(a.check_user_range(BASE, 3 * P).is_ok());
    // A trim needs no slot.
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe { a.unmap_free(BASE, P, &mut pool, &mut nop).unwrap() };
    assert_eq!(a.user_frames(), frames - 1);
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { a.teardown_pool(&mut pool) };
}

#[derive(Debug, PartialEq, Eq)]
enum Ev {
    Flush(u64),
    Free(u64),
}

/// A pool that logs each frame it takes back into a log it shares with
/// the flush.
struct Logging<'a> {
    pool: &'a mut Pool,
    log: &'a core::cell::RefCell<Vec<Ev>>,
}

// SAFETY: `free_frame` gives every frame back to `pool`'s buddy, which
// handed it out; established here.
unsafe impl FrameFree for Logging<'_> {
    fn free_frame(&mut self, f: Frames) {
        self.log.borrow_mut().push(Ev::Free(f.base()));
        self.pool.buddy.free(f);
    }
}

#[test]
fn munmap_flush_before_free() {
    let mut pool = Pool::new(128);
    let kernel = kernel_mapper(&mut pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut a = unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe { a.map_anon(BASE, 4 * P, UserPerms::RW, &mut pool).unwrap() };
    let pas: Vec<u64> = (0..4)
        .map(|i| {
            a.mapper()
                .translate(VirtAddr(BASE + i * P))
                .unwrap()
                .0
                .as_u64()
        })
        .collect();
    let log = core::cell::RefCell::new(Vec::new());
    {
        let mut lp = Logging {
            pool: &mut pool,
            log: &log,
        };
        let mut flush = |va: u64| log.borrow_mut().push(Ev::Flush(va));
        // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
        // established here.
        unsafe { a.unmap_free(BASE, 4 * P, &mut lp, &mut flush).unwrap() };
    }
    let log = log.into_inner();
    assert_eq!(log.len(), 8);
    for i in 0..4u64 {
        let f = log
            .iter()
            .position(|e| *e == Ev::Flush(BASE + i * P))
            .unwrap();
        let r = log
            .iter()
            .position(|e| *e == Ev::Free(pas[i as usize]))
            .unwrap();
        assert!(f < r, "page {i}: flush at {f}, free at {r}");
    }
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { a.teardown_pool(&mut pool) };
}

#[test]
fn addr_space_brk_grow_shrink() {
    let mut pool = Pool::new(128);
    let kernel = kernel_mapper(&mut pool);
    let before = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut a = unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    assert_eq!(a.brk_plan(BASE + P), BrkPlan::Current);
    a.set_brk_start(BASE + 0x10);
    let b = BASE + P;
    assert_eq!((a.brk_start(), a.brk()), (b, b));
    assert_eq!(a.brk_plan(0), BrkPlan::Current);
    assert_eq!(a.brk_plan(b - 1), BrkPlan::Current);
    assert_eq!(a.brk_plan(USER_MAP_END + 1), BrkPlan::Current);
    assert_eq!(a.brk_plan(b), BrkPlan::SamePage);
    // Grow into the first page, then within it, then across two more.
    let grow = |a: &mut AddressSpace<Arch>, pool: &mut Pool, want: u64| match a.brk_plan(want) {
        BrkPlan::Grow { va, len } => {
            a.heap_grow_check(va, len).unwrap();
            // SAFETY: the heap range `brk_plan` returned lies clear of every mapped page, and
            // `pool` hands out owned frames; established here.
            unsafe { a.map_pages(va, len, UserPerms::RW, pool).unwrap() };
            a.heap_grow_commit(va, len, want).unwrap();
        }
        BrkPlan::SamePage => a.set_brk(want),
        other => panic!("brk({want:#x}): {other:?}"),
    };
    grow(&mut a, &mut pool, b + 0x10);
    assert_eq!(a.brk_plan(b + 0x20), BrkPlan::SamePage);
    grow(&mut a, &mut pool, b + 0x20);
    grow(&mut a, &mut pool, b + 2 * P + 8);
    grow(&mut a, &mut pool, b + 3 * P);
    assert_eq!(a.brk(), b + 3 * P);
    let rs: Vec<Region> = a.regions().collect();
    assert_eq!(rs.len(), 1);
    assert_eq!((rs[0].start, rs[0].len), (b, 3 * P));
    assert_eq!(a.user_frames(), 3);
    // Shrink to one page: the two above go.
    assert_eq!(
        a.brk_plan(b + P),
        BrkPlan::Shrink {
            va: b + P,
            len: 2 * P
        }
    );
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe { a.unmap_free(b + P, 2 * P, &mut pool, &mut nop).unwrap() };
    a.set_brk(b + P);
    assert_eq!(a.user_frames(), 1);
    assert_eq!(a.regions().next().map(|r| r.len), Some(P));
    // Growth into another region leaves the break.
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe { a.map_anon(b + 2 * P, P, UserPerms::RW, &mut pool).unwrap() };
    assert_eq!(a.brk_plan(b + 3 * P), BrkPlan::Current);
    assert_eq!(a.heap_grow_check(b + P, 2 * P), Err(AsError::Overlap));
    // Shrinking to the start removes the heap region; growth adds it back.
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe { a.unmap_free(b, P, &mut pool, &mut nop).unwrap() };
    a.set_brk(b);
    assert_eq!(a.regions().count(), 1);
    grow(&mut a, &mut pool, b + 8);
    assert_eq!(a.regions().count(), 2);
    // A hole unmapped in the heap's middle stays a hole when it grows.
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe { a.unmap_free(b + 2 * P, P, &mut pool, &mut nop).unwrap() };
    grow(&mut a, &mut pool, b + P + 8);
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe { a.unmap_free(b + P, P, &mut pool, &mut nop).unwrap() };
    a.set_brk(b + 2 * P);
    grow(&mut a, &mut pool, b + 3 * P);
    let mut rs: Vec<(u64, u64)> = a.regions().map(|r| (r.start, r.len)).collect();
    rs.sort();
    assert_eq!(rs, [(b, P), (b + 2 * P, P)]);
    assert_eq!(a.user_frames(), 2);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut c = unsafe { a.clone_anon(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { c.teardown_pool(&mut pool) };
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { a.teardown_pool(&mut pool) };
    assert_eq!(used(&pool), before);
}

fn req(addr: u64, len: u64, fixed: Fixed) -> MmapReq {
    MmapReq {
        addr,
        len,
        perms: Some(UserPerms::RW),
        fixed,
    }
}

#[test]
fn addr_space_mmap_anon_placement() {
    let mut pool = Pool::new(128);
    let kernel = kernel_mapper(&mut pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut a = unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    assert_eq!(MMAP_TOP, 0x7FFF_F7FF_F000);
    let top = a.mmap_place(&req(0, 4 * P, Fixed::No)).unwrap();
    assert_eq!(top, MMAP_TOP - 4 * P);
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe { a.map_anon(top, 4 * P, UserPerms::RW, &mut pool).unwrap() };
    assert_eq!(a.mmap_place(&req(0, 2 * P, Fixed::No)), Ok(top - 2 * P));
    // A region at the top leaves a gap too small for 2 pages, which
    // placement skips.
    // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
    // anything, and `pool` hands out owned frames; established here.
    unsafe {
        a.map_anon(top - 3 * P, 2 * P, UserPerms::RW, &mut pool)
            .unwrap()
    };
    assert_eq!(a.mmap_place(&req(0, 2 * P, Fixed::No)), Ok(top - 5 * P));
    assert_eq!(a.mmap_place(&req(0, P, Fixed::No)), Ok(top - P));
    // A free hint is used, rounded down; a taken one is ignored.
    assert_eq!(a.mmap_place(&req(BASE + 5, P, Fixed::No)), Ok(BASE));
    assert_eq!(a.mmap_place(&req(top + 8, P, Fixed::No)), Ok(top - P));
    // Fixed requests.
    assert_eq!(a.mmap_place(&req(BASE, P, Fixed::Replace)), Ok(BASE));
    assert_eq!(
        a.mmap_place(&req(top, P, Fixed::NoReplace)),
        Err(AsError::Overlap)
    );
    assert_eq!(
        a.mmap_place(&req(top, P, Fixed::Replace)),
        Err(AsError::Overlap)
    );
    assert_eq!(
        a.mmap_place(&req(0, P, Fixed::Replace)),
        Err(AsError::NullGuard)
    );
    assert_eq!(
        a.mmap_place(&req(USER_MAP_END, P, Fixed::NoReplace)),
        Err(AsError::KernelRange)
    );
    // Too long for the space below `MMAP_TOP`.
    assert_eq!(
        a.mmap_place(&req(0, MMAP_TOP, Fixed::No)),
        Err(AsError::NoVaSpace)
    );
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { a.teardown_pool(&mut pool) };
}

#[test]
fn mmap_request_decodes_flags() {
    const PA: u64 = MAP_PRIVATE | MAP_ANONYMOUS;
    const RW: u64 = PROT_READ | PROT_WRITE;
    let ok = |prot, flags| mmap_request(0x1000, 10, prot, flags, 0);
    for extra in [
        0,
        MAP_NORESERVE,
        MAP_POPULATE,
        MAP_STACK,
        MAP_NORESERVE | MAP_POPULATE | MAP_STACK,
    ] {
        let r = ok(RW, PA | extra).unwrap();
        assert_eq!(r.len, P);
        assert_eq!(r.fixed, Fixed::No);
        assert_eq!(
            ok(RW, PA | extra | MAP_FIXED).unwrap().fixed,
            Fixed::Replace
        );
        assert_eq!(
            ok(RW, PA | extra | MAP_FIXED_NOREPLACE).unwrap().fixed,
            Fixed::NoReplace
        );
    }
    for (prot, perms) in [
        (0, None),
        (PROT_READ, Some(UserPerms::READ)),
        (PROT_WRITE, Some(UserPerms::RW)),
        (RW, Some(UserPerms::RW)),
        (PROT_EXEC, Some(UserPerms::RX)),
        (PROT_READ | PROT_EXEC, Some(UserPerms::RX)),
        (RW | PROT_EXEC, Some(UserPerms::RWX)),
    ] {
        assert_eq!(ok(prot, PA).unwrap().perms, perms, "prot {prot}");
    }
    // Each error, and that the earlier check wins.
    use MmapError::*;
    let cases: [(u64, u64, u64, u64, u64, MmapError); 11] = [
        (0, 0, 8, MAP_SHARED, 1, Inval),
        (0, 0, RW, MAP_PRIVATE, 0, NotAnon),
        (0, 0, 8, MAP_SHARED, 0, NotAnon),
        (0, 0, 8, MAP_SHARED | MAP_ANONYMOUS, 0, Inval),
        (0, P, RW, MAP_ANONYMOUS, 0, Inval),
        (0, P, RW, MAP_SHARED | MAP_ANONYMOUS, 0, Inval),
        (0, P, RW, PA | 0x100, 0, Inval),
        (0, P, 8, PA, 0, Inval),
        (0, 1 << 47, 8, PA, 0, Inval),
        (0, 1 << 47, RW, PA, 0, NoMem),
        (0x4000_0001, P, RW, PA | MAP_FIXED, 0, Inval),
    ];
    for (addr, len, prot, flags, off, want) in cases {
        assert_eq!(
            mmap_request(addr, len, prot, flags, off),
            Err(want),
            "addr {addr:#x} len {len:#x} prot {prot} flags {flags:#x} off {off}"
        );
    }
    assert_eq!(mmap_request(0, u64::MAX, RW, PA, 0), Err(NoMem));
    assert_eq!(
        mmap_request(0, USER_MAP_END, RW, PA, 0).map(|r| r.len),
        Ok(USER_MAP_END)
    );
    assert_eq!(
        mmap_request(0x4000_0001, P, RW, PA | MAP_FIXED_NOREPLACE, 0),
        Err(Inval)
    );
    assert_eq!(
        mmap_request(0x4000_0001, P, RW, PA, 0).map(|r| r.addr),
        Ok(0x4000_0001)
    );
}

/// Record a `PROT_NONE` reservation, as the kernel's `mmap` does.
fn reserve(a: &mut AddressSpace<Arch>, va: u64, len: u64) -> Result<(), AsError> {
    a.check_new_region(va, len)?;
    a.insert_region(Region {
        start: va,
        len,
        perms: UserPerms::READ,
        backing: Backing::Reserved,
    })
}

#[test]
fn prot_none_reserves_no_frames() {
    let mut pool = Pool::new(64);
    let kernel = kernel_mapper(&mut pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut a = unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    let after_new = used(&pool);
    reserve(&mut a, BASE, 16 * P).unwrap();
    assert_eq!(used(&pool), after_new);
    assert_eq!(a.user_frames(), 0);
    assert_eq!(a.check_user_range(BASE, 1), Err(UserMemError::Unmapped));
    assert_eq!(
        // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
        // anything, and `pool` hands out owned frames; established here.
        unsafe { a.map_anon(BASE + P, P, UserPerms::RW, &mut pool) },
        Err(AsError::Overlap)
    );
    assert_eq!(
        a.mmap_place(&req(0, 17 * P, Fixed::No)),
        Ok(MMAP_TOP - 17 * P)
    );
    // Unmapping splits the reservation and frees nothing.
    let mut flushes = 0;
    // SAFETY: every leaf in the range came from `pool`, and a host test has no TLB to flush;
    // established here.
    unsafe {
        a.unmap_free(BASE + 4 * P, 4 * P, &mut pool, &mut |_| flushes += 1)
            .unwrap()
    };
    assert_eq!(flushes, 0);
    assert_eq!(a.regions().count(), 2);
    assert_eq!(used(&pool), after_new);
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { a.teardown_pool(&mut pool) };
}

#[test]
fn clone_keeps_brk_and_reservations() {
    let mut pool = Pool::new(128);
    let kernel = kernel_mapper(&mut pool);
    let before = used(&pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut a = unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    a.set_brk_start(BASE);
    let BrkPlan::Grow { va, len } = a.brk_plan(BASE + 0x1800) else {
        panic!("no grow");
    };
    // SAFETY: the heap range `brk_plan` returned lies clear of every mapped page, and `pool` hands
    // out owned frames; established here.
    unsafe { a.map_pages(va, len, UserPerms::RW, &mut pool).unwrap() };
    a.heap_grow_commit(va, len, BASE + 0x1800).unwrap();
    a.write_bytes(BASE + 0x1000, b"heap").unwrap();
    reserve(&mut a, BASE + 16 * P, 4 * P).unwrap();
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut c = unsafe { a.clone_anon(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    assert_eq!((c.brk_start(), c.brk()), (BASE, BASE + 0x1800));
    assert_eq!(c.user_frames(), 2);
    let mut got = [0u8; 4];
    c.read_bytes(BASE + 0x1000, &mut got).unwrap();
    assert_eq!(&got, b"heap");
    let mut rs: Vec<(u64, u64, Backing)> =
        c.regions().map(|r| (r.start, r.len, r.backing)).collect();
    rs.sort_by_key(|r| r.0);
    assert_eq!(
        rs,
        [
            (BASE, 2 * P, Backing::Anonymous),
            (BASE + 16 * P, 4 * P, Backing::Reserved)
        ]
    );
    assert_eq!(
        c.check_user_range(BASE + 16 * P, 1),
        Err(UserMemError::Unmapped)
    );
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe {
        c.teardown_pool(&mut pool);
        a.teardown_pool(&mut pool);
    }
    assert_eq!(used(&pool), before);
}

/// The region table is a fixed `MAX_REGIONS` slots (ROADMAP §10.4, D1):
/// with every slot holding a region no neighbour can merge with, the next
/// `map_anon` gets `NoRegionSlot` and maps nothing.
#[test]
fn region_table_full_is_no_region_slot() {
    let mut pool = Pool::new(MAX_REGIONS + 128);
    let kernel = kernel_mapper(&mut pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let mut a = unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    let mut va = BASE;
    let mut n = 0usize;
    while n < MAX_REGIONS {
        // SAFETY: `map_anon` refuses a range outside the user half or over a region before it maps
        // anything, and `pool` hands out owned frames; established here.
        unsafe { a.map_anon(va, P, UserPerms::RW, &mut pool).unwrap() };
        va += 2 * P;
        n += 1;
    }
    assert_eq!(a.regions().count(), MAX_REGIONS);
    let frames = a.user_frames();
    let used_before = used(&pool);
    assert_eq!(
        // SAFETY: as above; established here.
        unsafe { a.map_anon(va, P, UserPerms::RW, &mut pool) },
        Err(AsError::NoRegionSlot)
    );
    assert_eq!(a.user_frames(), frames);
    assert_eq!(used(&pool), used_before);
    // SAFETY: a host test loads no CR3, the space is not used again, and every frame it holds came
    // from `pool`; established here.
    unsafe { a.teardown_pool(&mut pool) };
}

#[test]
fn fixed_tables_match_limits() {
    let mut pool = Pool::new(64);
    let kernel = kernel_mapper(&mut pool);
    // SAFETY: `kernel` is this test's kernel mapper and `pool` hands out owned frames writable
    // through its HHDM; established here.
    let aspace =
        unsafe { AddressSpace::new(&kernel, &mut pool, &mut region_table().ok()) }.unwrap();
    assert_eq!(aspace.regions.len(), crate::limits::MAX_REGIONS);
}
