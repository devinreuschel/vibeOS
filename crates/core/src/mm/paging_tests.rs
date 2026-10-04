//! Host tests for `mm::paging`.

use super::*;
use crate::arch::stub::Arch;
use crate::pmm::testing::Pool;

#[test]
fn user_map_end_is_top_page_below_user_end() {
    assert_eq!(USER_MAP_END, 0x0000_7FFF_FFFF_F000);
    assert_eq!(USER_END - USER_MAP_END, PAGE_SIZE_4K);
    assert!(is_canonical(USER_MAP_END) && !is_canonical(USER_END));
}

/// A mapper over a fresh root from `pool`. The root's token moves
/// into the mapper, which the test never tears down.
fn fresh_mapper(pool: &mut Pool) -> Mapper<Arch> {
    let root = PhysAddr(pool.alloc_frame().unwrap().into_entry());
    // Zero via the HHDM physmap.
    let ptr = root.0.wrapping_add(pool.hhdm()) as *mut u64;
    for i in 0..<Arch as PageTable>::ENTRIES {
        // SAFETY: `root` is an order-0 frame of the pool's host memory,
        // reached at `pool.hhdm()`, and `i < A::ENTRIES` words stay inside it,
        // established here.
        unsafe { ptr.add(i).write_volatile(0) };
    }
    // SAFETY: `Mapper::new`'s contract; `root` is the zeroed pool frame
    // above, which the mapper keeps, and the pool's offset reaches every
    // frame it hands out, established here.
    unsafe { Mapper::new(root, pool.hhdm()) }
}

#[test]
fn canonical_check() {
    assert!(is_canonical(0x0000_7FFF_FFFF_FFFF));
    assert!(is_canonical(0xFFFF_8000_0000_0000));
    assert!(is_canonical(0));
    // Middle of the non-canonical hole.
    assert!(!is_canonical(0x0000_8000_0000_0000));
    assert!(!is_canonical(0xFFFF_7FFF_FFFF_FFFF));
}

#[test]
fn map_translate_unmap_4k() {
    let mut pool = Pool::new(64);
    let mut m = fresh_mapper(&mut pool);
    let va = VirtAddr(0xFFFF_C000_0010_0000);
    let pa = PhysAddr(0x0080_0000);
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    unsafe {
        m.map_page(
            va,
            pa,
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap();
    }
    let (got, size, flags) = m.translate(va).unwrap();
    assert_eq!(got, pa);
    assert_eq!(size, PageSize::Size4K);
    assert!(flags.contains(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NX));
    // Offset within the page carries through.
    let (got_mid, _, _) = m.translate(VirtAddr(va.0 + 0x123)).unwrap();
    assert_eq!(got_mid.0, pa.0 + 0x123);
    // Unmap returns the frame and translate is None.
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    let (unm_pa, unm_size) = unsafe { m.unmap_page(va).unwrap() };
    assert_eq!(unm_pa, pa);
    assert_eq!(unm_size, PageSize::Size4K);
    assert!(m.translate(va).is_none());
}

#[test]
fn map_translate_2m() {
    let mut pool = Pool::new(64);
    let mut m = fresh_mapper(&mut pool);
    let va = VirtAddr(0xFFFF_8000_0020_0000);
    let pa = PhysAddr(0x0040_0000);
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    unsafe {
        m.map_page(
            va,
            pa,
            physmap_flags(),
            PageSize::Size2M,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap();
    }
    let (got, size, _) = m.translate(va).unwrap();
    assert_eq!(got, pa);
    assert_eq!(size, PageSize::Size2M);
    // Offset well inside the 2M page.
    let mid = VirtAddr(va.0 + 0x0010_0000);
    let (mid_pa, _, _) = m.translate(mid).unwrap();
    assert_eq!(mid_pa.0, pa.0 + 0x0010_0000);
}

#[test]
fn misaligned_2m_rejected() {
    let mut pool = Pool::new(64);
    let mut m = fresh_mapper(&mut pool);
    let va = VirtAddr(0xFFFF_8000_0020_1000); // not 2M-aligned
    let pa = PhysAddr(0x0040_0000);
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    let err = unsafe {
        m.map_page(
            va,
            pa,
            physmap_flags(),
            PageSize::Size2M,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap_err()
    };
    assert_eq!(err, MapError::Misaligned);
}

#[test]
fn map_range_selects_2m_when_aligned() {
    let mut pool = Pool::new(256);
    let mut m = fresh_mapper(&mut pool);
    let va = VirtAddr(0xFFFF_8000_0000_0000);
    let pa = PhysAddr(0);
    // 6 MiB: three whole 2M pages, no tail.
    // SAFETY: `Mapper::map_range`'s contract; the host never touches the leaves' frames,
    // and the tables are the pool's (`fresh_mapper`), established here.
    unsafe {
        m.map_range(
            va,
            pa,
            6 * PAGE_SIZE_2M,
            physmap_flags(),
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap();
    }
    for i in 0..3 {
        let (got, size, _) = m.translate(VirtAddr(va.0 + i * PAGE_SIZE_2M)).unwrap();
        assert_eq!(size, PageSize::Size2M, "block {i} should be 2M");
        assert_eq!(got.0, pa.0 + i * PAGE_SIZE_2M);
    }
}

#[test]
fn map_range_splits_head_tail_to_4k() {
    let mut pool = Pool::new(1024);
    let mut m = fresh_mapper(&mut pool);
    // Start at a 4K boundary that isn't 2M aligned, so the first
    // stretch has to be 4K until we roll into a 2M boundary; then
    // 2M pages; then 4K tail.
    let va = VirtAddr(0xFFFF_8000_0000_0000 + PAGE_SIZE_4K);
    let pa = PhysAddr(PAGE_SIZE_4K);
    let len = 3 * PAGE_SIZE_2M;
    // SAFETY: `Mapper::map_range`'s contract; the host never touches the leaves' frames,
    // and the tables are the pool's (`fresh_mapper`), established here.
    unsafe {
        m.map_range(va, pa, len, physmap_flags(), MapMode::Fresh, &mut pool)
            .unwrap();
    }
    // Spot check a page in the head (first 4K), one deep inside a
    // 2M block, and one in the tail.
    assert_eq!(m.translate(va).unwrap().0, pa);
    let mid = VirtAddr(0xFFFF_8000_0000_0000 + PAGE_SIZE_2M);
    assert_eq!(m.translate(mid).unwrap().1, PageSize::Size2M);
    let end = VirtAddr(va.0 + len - PAGE_SIZE_4K);
    assert!(m.translate(end).is_some());
}

#[test]
fn overlap_without_remap_errors() {
    let mut pool = Pool::new(64);
    let mut m = fresh_mapper(&mut pool);
    let va = VirtAddr(0xFFFF_C000_0000_0000);
    let pa = PhysAddr(0x0080_0000);
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    unsafe {
        m.map_page(
            va,
            pa,
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap();
    }
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    let err = unsafe {
        m.map_page(
            va,
            PhysAddr(0x0090_0000),
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap_err()
    };
    assert_eq!(err, MapError::AlreadyMapped);
    // Remap of a different PA is a live change.
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    let err = unsafe {
        m.map_page(
            va,
            PhysAddr(0x0090_0000),
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Remap,
            &mut pool,
        )
        .unwrap_err()
    };
    assert_eq!(err, MapError::LiveChange);
    assert_eq!(m.translate(va).unwrap().0, pa);
    // Invalidate then Fresh (BBM) installs the new PA.
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    unsafe {
        m.map_page(
            va,
            PhysAddr(0x0090_0000),
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Invalidate,
            &mut pool,
        )
        .unwrap();
    }
    assert_eq!(m.translate(va).unwrap().0, PhysAddr(0x0090_0000));
}

#[test]
fn page_size_mismatch_rejected() {
    let mut pool = Pool::new(64);
    let mut m = fresh_mapper(&mut pool);
    let va = VirtAddr(0xFFFF_8000_0040_0000);
    // Place a 2M leaf.
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    unsafe {
        m.map_page(
            va,
            PhysAddr(0x0080_0000),
            physmap_flags(),
            PageSize::Size2M,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap();
    }
    // Attempt a 4K page inside the 2M region: the L2 slot is a huge
    // leaf, so the walk to L1 fails with PageSizeMismatch.
    let inside = VirtAddr(va.0 + PAGE_SIZE_4K);
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    let err = unsafe {
        m.map_page(
            inside,
            PhysAddr(0x0090_0000),
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap_err()
    };
    assert_eq!(err, MapError::PageSizeMismatch);
}

fn map_ok(
    m: &mut Mapper<Arch>,
    pool: &mut Pool,
    va: VirtAddr,
    pa: PhysAddr,
    flags: PageFlags,
    size: PageSize,
    mode: MapMode,
) -> Result<(), MapError> {
    // SAFETY: host tables; the leaf frame is never touched; established here.
    unsafe { m.map_page(va, pa, flags, size, mode, pool) }
}

#[test]
fn remap_refuses_each_live_change() {
    let mut pool = Pool::new(64);
    let mut m = fresh_mapper(&mut pool);
    let va = VirtAddr(0xFFFF_C000_0000_0000);
    let pa = PhysAddr(0x0080_0000);
    map_ok(
        &mut m,
        &mut pool,
        va,
        pa,
        physmap_flags(),
        PageSize::Size4K,
        MapMode::Fresh,
    )
    .unwrap();

    assert_eq!(
        map_ok(
            &mut m,
            &mut pool,
            va,
            PhysAddr(0x0090_0000),
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Remap,
        ),
        Err(MapError::LiveChange)
    );
    assert_eq!(
        map_ok(
            &mut m,
            &mut pool,
            va,
            pa,
            mmio_flags(),
            PageSize::Size4K,
            MapMode::Remap,
        ),
        Err(MapError::LiveChange)
    );
    assert_eq!(
        map_ok(
            &mut m,
            &mut pool,
            va,
            pa,
            physmap_flags().with(PageFlags::CONTIGUOUS),
            PageSize::Size4K,
            MapMode::Remap,
        ),
        Err(MapError::LiveChange)
    );
    let noglob = physmap_flags().without(PageFlags::GLOBAL);
    map_ok(
        &mut m,
        &mut pool,
        va,
        pa,
        noglob,
        PageSize::Size4K,
        MapMode::Invalidate,
    )
    .unwrap();
    assert_eq!(
        map_ok(
            &mut m,
            &mut pool,
            va,
            pa,
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Remap,
        ),
        Err(MapError::LiveChange)
    );
    // Permission-only and global → nG are allowed.
    map_ok(
        &mut m,
        &mut pool,
        va,
        pa,
        noglob.without(PageFlags::WRITABLE),
        PageSize::Size4K,
        MapMode::Remap,
    )
    .unwrap();
    assert!(!m.translate(va).unwrap().2.contains(PageFlags::WRITABLE));
    map_ok(
        &mut m,
        &mut pool,
        va,
        pa,
        noglob,
        PageSize::Size4K,
        MapMode::Remap,
    )
    .unwrap();
}

#[test]
fn map_page_1g() {
    let mut pool = Pool::new(64);
    let mut m = fresh_mapper(&mut pool);
    let va = VirtAddr(0xFFFF_8000_0000_0000);
    let pa = PhysAddr(0);
    map_ok(
        &mut m,
        &mut pool,
        va,
        pa,
        physmap_flags(),
        PageSize::Size1G,
        MapMode::Fresh,
    )
    .unwrap();
    let (got, size, _) = m.translate(va).unwrap();
    assert_eq!(got, pa);
    assert_eq!(size, PageSize::Size1G);
    let mid = VirtAddr(va.0 + 0x2000_0000);
    assert_eq!(m.translate(mid).unwrap().0, PhysAddr(0x2000_0000));
}

#[test]
fn ioremap_bumps_and_bounds() {
    let mut w = IoremapWindow::new();
    // Simple aligned reservation.
    let a = w.reserve(PhysAddr(0xFEE0_0000), 4096).unwrap();
    assert_eq!(a.0 & (PAGE_SIZE_4K - 1), 0);
    // Sub-page tail: reservation still returns the exact phys offset
    // so device access hits the right byte.
    let b = w.reserve(PhysAddr(0xFEC0_0123), 0x100).unwrap();
    assert_eq!(b.0 & 0xFFF, 0x123);
    // Exhaustion returns None.
    let mut w = IoremapWindow::new();
    assert!(w.reserve(PhysAddr(0), IOREMAP_LEN + 4096).is_none());
}

#[test]
fn ioremap_unreserve_returns_only_the_latest() {
    let mut w = IoremapWindow::new();
    let a = w.reserve(PhysAddr(0xFEB0_0000), 0x2000).unwrap();
    let after_a = w.next();
    let b = w.reserve(PhysAddr(0xFEC0_0123), 0x1F00).unwrap();
    // `a` is not the latest: refused, and the cursor stays.
    assert!(!w.unreserve(a, 0x2000));
    assert_eq!(w.next(), after_a + 0x3000);
    // A wrong length is refused too.
    assert!(!w.unreserve(b, 0x100));
    assert!(w.unreserve(b, 0x1F00));
    assert_eq!(w.next(), after_a);
    // The VA goes out again, at the same offset into its page.
    let c = w.reserve(PhysAddr(0xFED0_0123), 0x1F00).unwrap();
    assert_eq!(c, b);
    assert!(w.unreserve(c, 0x1F00));
    assert!(w.unreserve(a, 0x2000));
    assert_eq!(w.next(), IOREMAP_BASE);
}

#[test]
fn walk_ranges_coalesces_and_skips_holes() {
    let mut pool = Pool::new(256);
    let mut m = fresh_mapper(&mut pool);
    let a = VirtAddr(0xFFFF_C000_0000_0000);
    let b = VirtAddr(0xFFFF_C000_0020_0000); // 2 MiB later
    // SAFETY: `Mapper::map_page`'s contract; the host never touches the leaf's frame, and
    // the tables are the pool's (`fresh_mapper`), established here.
    unsafe {
        m.map_page(
            a,
            PhysAddr(0x0080_0000),
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap();
        m.map_page(
            VirtAddr(a.0 + PAGE_SIZE_4K),
            PhysAddr(0x0080_1000),
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap();
        m.map_page(
            b,
            PhysAddr(0x0090_0000),
            physmap_flags(),
            PageSize::Size4K,
            MapMode::Fresh,
            &mut pool,
        )
        .unwrap();
    }
    let mut ranges = Vec::new();
    m.walk_ranges(
        VirtAddr(0xFFFF_C000_0000_0000),
        VirtAddr(0xFFFF_C000_0040_0000),
        |va, len, _, size| {
            ranges.push((va, len, size));
        },
    );
    assert_eq!(ranges.len(), 2, "two 4K pages should coalesce; hole splits");
    assert_eq!(ranges[0], (a.0, 2 * PAGE_SIZE_4K, PageSize::Size4K));
    assert_eq!(ranges[1], (b.0, PAGE_SIZE_4K, PageSize::Size4K));
    assert!(m.range_unmapped(
        VirtAddr(0xFFFF_D000_0000_0000),
        VirtAddr(0xFFFF_D000_0010_0000)
    ));
    assert!(!m.range_unmapped(a, VirtAddr(a.0 + PAGE_SIZE_4K)));
}
