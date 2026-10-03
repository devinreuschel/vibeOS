//! In-guest tests for mm (kernel_tests only). Rows: [`TESTS`].

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use vibeos::arch::PageTable;
use vibeos::heap::HEAP_SIZE;
use vibeos::kva::{KVA_END, KVA_START, PAGE_SIZE};
use vibeos::limits::MAX_UNMAP_PAGES;
use vibeos::paging::{self, PageFlags, PageSize, PhysAddr, VirtAddr, heap_flags};
use vibeos::pmm::Frames;

use crate::arch::current::Arch;
use crate::diag;
use crate::ktest::{
    Outcome, Test, alloc_frame, alloc_frames_owned, catch_alloc_error, catch_fault, free_frame,
    free_frames, free_frames_owned, quiescent_free_frames, second_cpu, settle_threads,
    spawn_thread_on, spin_until_ns, test,
};
use crate::kva_init;
use crate::paging_init;
use crate::per_cpu_init;
use crate::thread_init;

// ------------------ tests ------------------

pub(crate) fn test_map_unmap() -> Outcome {
    let Some(va) = alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let Some(pa) = alloc_frame() else {
        free_va(va, PAGE_SIZE);
        return Outcome::Fail("frame alloc");
    };
    // SAFETY: `paging_init::map_4k`'s contract; `pa` is a frame this test owns and `va` a KVA
    // page `alloc_va` reserved that nothing else maps; established here.
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        free_va(va, PAGE_SIZE);
        return Outcome::Fail("map_4k");
    }
    // SAFETY: `va` is mapped writable to memory this test owns (the map above); established
    // here.
    unsafe { (va.as_u64() as *mut u64).write_volatile(0xAABB_CCDD_EEFF_0011) };
    // SAFETY: `va` is mapped writable to memory this test owns (the map above); established
    // here.
    let got = unsafe { (va.as_u64() as *const u64).read_volatile() };
    if got != 0xAABB_CCDD_EEFF_0011 {
        return Outcome::Fail("readback mismatch");
    }
    // SAFETY: `unmap_4k`'s contract; `va` is a test page, neither a stack nor code nor heap,
    // and the test does not touch it again until it frees or remaps it; established here.
    let Some((unmapped, _)) = (unsafe { unmap_4k(va) }) else {
        return Outcome::Fail("unmap returned none");
    };
    if unmapped != pa {
        return Outcome::Fail("unmap phys mismatch");
    }
    free_frame(pa);
    free_va(va, PAGE_SIZE);
    // SAFETY: the access faults on purpose on an unmapped page; `catch_fault` recovers from the
    // #PF, established here.
    let fault = catch_fault(|| unsafe {
        (va.as_u64() as *mut u8).write_volatile(1);
    });
    match fault {
        Some(_) => Outcome::Ok,
        None => Outcome::Fail("access after unmap did not fault"),
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn test_nx_enforcement() -> Outcome {
    let Some(va) = alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let Some(pa) = alloc_frame() else {
        free_va(va, PAGE_SIZE);
        return Outcome::Fail("frame alloc");
    };
    // SAFETY: `paging_init::map_4k`'s contract; `pa` is a frame this test owns and `va` a KVA
    // page `alloc_va` reserved that nothing else maps; established here.
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        free_va(va, PAGE_SIZE);
        return Outcome::Fail("map_4k");
    }
    // SAFETY: `va` is mapped writable to memory this test owns (the map above); established
    // here.
    unsafe { (va.as_u64() as *mut u8).write_volatile(0xC3) };
    // SAFETY: a code address fits a fn pointer; calling it faults on the NX page before any
    // instruction runs, which `catch_fault` recovers from; established here.
    let f: unsafe extern "C" fn() = unsafe { core::mem::transmute(va.as_u64()) };
    core::hint::black_box(f);
    // SAFETY: executes the NX page on purpose; `catch_fault` recovers from the #PF, established
    // here.
    let fault = catch_fault(|| unsafe { f() });
    // SAFETY: `unmap_4k`'s contract; `va` is a test page, neither a stack nor code nor heap,
    // and the test does not touch it again until it frees or remaps it; established here.
    let _ = unsafe { unmap_4k(va) };
    free_frame(pa);
    free_va(va, PAGE_SIZE);
    let Some(fault) = fault else {
        return Outcome::Fail("NX execute did not fault");
    };
    // Error-code bit 4 is instruction-fetch (Intel SDM).
    if fault.error & (1 << 4) == 0 {
        crate::marker!(
            "vibeOS: ktest:   nx err={:#x} cr2={:#x}",
            fault.error,
            fault.cr2
        );
        return Outcome::Fail("PF was not instruction-fetch");
    }
    Outcome::Ok
}

pub(crate) fn test_heap_box() -> Outcome {
    let b = Box::new(0xDEAD_BEEFu64);
    if *b != 0xDEAD_BEEF {
        return Outcome::Fail("box payload");
    }
    drop(b);
    Outcome::Ok
}

pub(crate) fn test_heap_growth() -> Outcome {
    let mut v = Vec::new();
    v.resize(2 * 1024 * 1024, 0xABu8);
    if v.len() != 2 * 1024 * 1024 {
        return Outcome::Fail("vec len");
    }
    if v[0] != 0xAB || v[v.len() - 1] != 0xAB {
        return Outcome::Fail("vec pattern");
    }
    drop(v);
    Outcome::Ok
}

pub(crate) fn test_heap_align() -> Outcome {
    let mut align = 1usize;
    while align <= 4096 {
        let Ok(layout) = Layout::from_size_align(align, align) else {
            return Outcome::Fail("layout");
        };
        // SAFETY: `GlobalAlloc::alloc`'s contract; the layout has a nonzero size; established
        // here.
        let p = unsafe { alloc::alloc::alloc(layout) };
        if p.is_null() {
            return Outcome::Fail("alloc null");
        }
        if !(p as usize).is_multiple_of(align) {
            // SAFETY: `GlobalAlloc::dealloc`'s contract; each pointer came from `alloc` with
            // this layout and is not used after; established here.
            unsafe { alloc::alloc::dealloc(p, layout) };
            return Outcome::Fail("alignment");
        }
        // SAFETY: `p` is a live allocation of `align >= 1` bytes; established here.
        unsafe { p.write(0x5A) };
        // SAFETY: `GlobalAlloc::dealloc`'s contract; each pointer came from `alloc` with this
        // layout and is not used after; established here.
        unsafe { alloc::alloc::dealloc(p, layout) };
        align *= 2;
    }
    Outcome::Ok
}

pub(crate) fn test_heap_reuse() -> Outcome {
    let Ok(small) = Layout::from_size_align(16, 8) else {
        return Outcome::Fail("layout");
    };
    let Ok(layout) = Layout::from_size_align(64, 8) else {
        return Outcome::Fail("layout");
    };
    // Sandwich: live blocks on both sides so the hole cannot coalesce
    // with a larger neighbour (first-fit would then carve a different VA).
    // SAFETY: `GlobalAlloc::alloc`'s contract; the layout has a nonzero size; established here.
    let pad = unsafe { alloc::alloc::alloc(small) };
    // SAFETY: `GlobalAlloc::alloc`'s contract; the layout has a nonzero size; established here.
    let a = unsafe { alloc::alloc::alloc(layout) };
    // SAFETY: `GlobalAlloc::alloc`'s contract; the layout has a nonzero size; established here.
    let keep = unsafe { alloc::alloc::alloc(layout) };
    if pad.is_null() || a.is_null() || keep.is_null() {
        return Outcome::Fail("setup alloc");
    }
    // SAFETY: `GlobalAlloc::dealloc`'s contract; each pointer came from `alloc` with this
    // layout and is not used after; established here.
    unsafe { alloc::alloc::dealloc(a, layout) };
    // SAFETY: `GlobalAlloc::alloc`'s contract; the layout has a nonzero size; established here.
    let b = unsafe { alloc::alloc::alloc(layout) };
    if b.is_null() {
        return Outcome::Fail("second alloc");
    }
    // Compare as usize through black_box: LLVM treats GlobalAlloc like
    // malloc and will fold `a == b` after free at opt-level 1.
    let reused = core::hint::black_box(a as usize) == core::hint::black_box(b as usize);
    if !reused {
        crate::marker!("vibeOS: ktest:   reuse pad={pad:p} a={a:p} keep={keep:p} b={b:p}");
        // SAFETY: `GlobalAlloc::dealloc`'s contract; each pointer came from `alloc` with this
        // layout and is not used after; established here.
        unsafe {
            alloc::alloc::dealloc(b, layout);
            alloc::alloc::dealloc(keep, layout);
            alloc::alloc::dealloc(pad, small);
        };
        return Outcome::Fail("did not reuse freed block");
    }
    // SAFETY: `GlobalAlloc::realloc`'s contract; `b` came from `alloc` with `layout`, and 32
    // bytes at its alignment fit `isize`; established here.
    let c = unsafe { alloc::alloc::realloc(b, layout, 32) };
    let same = core::hint::black_box(c as usize) == core::hint::black_box(b as usize);
    if !c.is_null() {
        // SAFETY: `GlobalAlloc::dealloc`'s contract; each pointer came from `alloc` with this
        // layout and is not used after; established here.
        unsafe { alloc::alloc::dealloc(c, Layout::from_size_align(32, 8).unwrap()) };
    }
    // SAFETY: `GlobalAlloc::dealloc`'s contract; each pointer came from `alloc` with this
    // layout and is not used after; established here.
    unsafe { alloc::alloc::dealloc(keep, layout) };
    // SAFETY: `GlobalAlloc::dealloc`'s contract; each pointer came from `alloc` with this
    // layout and is not used after; established here.
    unsafe { alloc::alloc::dealloc(pad, small) };
    if !same {
        return Outcome::Fail("realloc shrink moved");
    }
    Outcome::Ok
}

pub(crate) fn test_heap_oom() -> Outcome {
    let hit = catch_alloc_error(|| {
        let _v: Vec<u8> = Vec::with_capacity((HEAP_SIZE as usize) + 4096);
    });
    if !hit {
        return Outcome::Fail("error handler not reached");
    }
    let b = Box::new(1u32);
    if *b != 1 {
        return Outcome::Fail("heap unusable after oom");
    }
    Outcome::Ok
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn test_stack_guard() -> Outcome {
    let Ok(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    // SAFETY: `stack`'s lowest mapped page is writable (`kva_init::alloc_guarded_stack`) and no
    // thread runs on it; established here.
    unsafe { (stack.base().as_u64() as *mut u64).write_volatile(0x1111_2222) };
    // SAFETY: `stack`'s lowest mapped page is writable (`kva_init::alloc_guarded_stack`) and no
    // thread runs on it; established here.
    let got = unsafe { (stack.base().as_u64() as *const u64).read_volatile() };
    if got != 0x1111_2222 {
        kva_init::free_stack(stack);
        return Outcome::Fail("mapped stack not writable");
    }
    let guard = stack.guard().as_u64();
    // SAFETY: the access faults on purpose on an unmapped page; `catch_fault` recovers from the
    // #PF, established here.
    let fault = catch_fault(|| unsafe {
        (guard as *mut u8).write_volatile(1);
    });
    kva_init::free_stack(stack);
    match fault {
        Some(f) if (f.cr2 & !0xFFF) == (guard & !0xFFF) => Outcome::Ok,
        Some(_) => Outcome::Fail("fault cr2 was not the guard page"),
        None => Outcome::Fail("guard write did not fault"),
    }
}

pub(crate) fn test_kva_roundtrip() -> Outcome {
    let before = quiescent_free_frames();
    let Ok(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    let mid = free_frames();
    if mid + 4 != before {
        kva_init::free_stack(stack);
        crate::marker!("vibeOS: ktest:   before={before} mid={mid}");
        return Outcome::Fail("stack did not take 4 frames");
    }
    kva_init::free_stack(stack);
    let after = quiescent_free_frames();
    if after != before {
        crate::marker!("vibeOS: ktest:   before={before} after={after}");
        return Outcome::Fail("free did not restore frame count");
    }
    Outcome::Ok
}

/// A stack parked on this CPU's dead list comes back through its worker
/// (ROADMAP §10.10).
pub(crate) fn test_kva_deferred() -> Outcome {
    let before = quiescent_free_frames();
    let Ok(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    if free_frames() + 4 != before {
        kva_init::free_stack(stack);
        return Outcome::Fail("stack did not take 4 frames");
    }
    // SAFETY: invariant I10: the stack was allocated just above and no
    // thread was given it; established here.
    unsafe { thread_init::testing::park_on_local_list(stack) };
    let after = quiescent_free_frames();
    if after != before {
        crate::marker!("vibeOS: ktest:   before={before} after={after}");
        return Outcome::Fail("worker did not free the parked stack");
    }
    Outcome::Ok
}

pub(crate) fn test_vmap() -> Outcome {
    let Some(f) = alloc_frames_owned(1) else {
        return Outcome::Fail("frames");
    };
    // `vmap` frees the frames itself when it fails.
    let Ok(v) = kva_init::vmap(f) else {
        return Outcome::Fail("vmap");
    };
    let va = v.base();
    // SAFETY: `va` is mapped writable to memory this test owns (the map above); established
    // here.
    unsafe { (va.as_u64() as *mut u64).write_volatile(0x100) };
    // SAFETY: `va` is mapped writable to memory this test owns (the map above); established
    // here.
    unsafe { ((va.as_u64() + PAGE_SIZE) as *mut u64).write_volatile(0x200) };
    // SAFETY: `va` is mapped writable to memory this test owns (the map above); established
    // here.
    let ga = unsafe { (va.as_u64() as *const u64).read_volatile() };
    // SAFETY: `va` is mapped writable to memory this test owns (the map above); established
    // here.
    let gb = unsafe { ((va.as_u64() + PAGE_SIZE) as *const u64).read_volatile() };
    free_frames_owned(kva_init::vunmap(v));
    if ga != 0x100 || gb != 0x200 {
        return Outcome::Fail("vmap readback");
    }
    Outcome::Ok
}

/// A `FrameAlloc` that has no frames: remapping a present leaf needs no
/// table, so `mmio_uc_flags`' restore never asks it for one.
struct NoFrames;

// SAFETY: `FrameAlloc`'s contract is on the frames it hands out, and this
// one hands out none; established here.
unsafe impl paging::FrameAlloc for NoFrames {
    fn alloc_frame(&mut self) -> Option<Frames> {
        None
    }
}

pub(crate) fn test_mmio_uc_flags() -> Outcome {
    // LAPIC (0xFEE0_0000) sits above QEMU's 128 MiB map_end, so the
    // generic patch API is still proven on a leaf we know exists:
    // 2 MiB, inside the identity rest / physmap. ACPI's real bases
    // are checked by `acpi_discovery`.
    let phys = PhysAddr(0x0020_0000);
    let va = VirtAddr(paging_init::HHDM_BASE + phys.as_u64());
    // The whole leaf the patch covers, and its flags, to restore after.
    let Some((pa, size, saved)) = paging_init::translate(va) else {
        return Outcome::Fail("translate before");
    };
    let mask = size.bytes() - 1;
    let leaf_va = VirtAddr(va.as_u64() & !mask);
    let leaf_pa = PhysAddr(pa.as_u64() & !mask);
    // SAFETY: `paging_init::patch_physmap_uc`'s contract; `install` has run and the physmap
    // covers physical 2 MiB. Making that RAM leaf UC breaks invariant I17 on purpose, for this
    // test, until the restore below: UC only slows its accesses; established here.
    if unsafe { paging_init::patch_physmap_uc(phys, 4096) }.is_err() {
        return Outcome::Fail("patch_physmap_uc");
    }
    let patched = paging_init::translate(va).map(|(_, _, f)| f);
    let restored = {
        let mut m = paging_init::current_mapper();
        // SAFETY: `Mapper::map_page`'s contract; the leaf goes back to the
        // physical range and flags `translate` read above, which the
        // physmap mapped before this test, so no other mapping reaches it
        // anew; the guard holds the page-table lock (invariant I48,
        // established at `mm::paging_init::current_mapper`).
        unsafe {
            m.map_page(
                leaf_va,
                leaf_pa,
                saved,
                size,
                paging::MapMode::Remap,
                &mut NoFrames,
            )
        }
    };
    <Arch as PageTable>::flush_local(leaf_va);
    paging::tlb_shootdown_others(leaf_va);
    if restored.is_err() {
        return Outcome::Fail("restore map_page");
    }
    match patched {
        None => return Outcome::Fail("translate after patch"),
        Some(f) if !f.contains(PageFlags::PCD | PageFlags::PWT) => {
            return Outcome::Fail("PCD/PWT not set on physmap leaf");
        }
        Some(_) => {}
    }
    match paging_init::translate(va) {
        Some((_, s, f)) if s == size && f.0 == saved.0 => Outcome::Ok,
        Some((_, _, f)) => crate::fail_fmt!("restored flags {:#x}, want {:#x}", f.0, saved.0),
        None => Outcome::Fail("translate after restore"),
    }
}

/// The probe `tlb_shootdown_remote` drives on the AP: a kernel thread
/// pinned there, fed through these statics rather than call-function work,
/// which takes no lock (DESIGN §2.2) while `catch_fault` takes
/// `arch::catch::LAST`. `SHOOT_REQ` counts requests (`SHOOT_QUIT` ends the
/// thread), `SHOOT_ACK` names the last one served, and `SHOOT_RESULT` holds
/// its outcome: 1 the read succeeded, 2 it faulted.
static SHOOT_VA: AtomicU64 = AtomicU64::new(0);
static SHOOT_REQ: AtomicU64 = AtomicU64::new(0);
static SHOOT_ACK: AtomicU64 = AtomicU64::new(0);
static SHOOT_RESULT: AtomicU64 = AtomicU64::new(0);
const SHOOT_QUIT: u64 = u64::MAX;

/// Spin on the AP, never blocking, so the TLB entry a read loads stays
/// live until a shootdown removes it; IF stays on for the shootdown IPI.
fn shoot_prober() {
    let mut seen = 0u64;
    loop {
        // Acquire: pairs with the Release store in `shoot_touch`.
        let req = SHOOT_REQ.load(Ordering::Acquire);
        if req == SHOOT_QUIT {
            SHOOT_ACK.store(SHOOT_QUIT, Ordering::Release);
            return;
        }
        if req != seen {
            let va = SHOOT_VA.load(Ordering::Relaxed);
            // SAFETY: `va` is the test's page, mapped to a frame the test
            // owns or, after its unmap, unmapped; `catch_fault` recovers
            // from the #PF in the second case, established here.
            let fault = catch_fault(|| unsafe {
                core::ptr::read_volatile(va as *const u64);
            });
            SHOOT_RESULT.store(if fault.is_some() { 2 } else { 1 }, Ordering::Relaxed);
            seen = req;
            // Release: publishes the result to `shoot_touch`.
            SHOOT_ACK.store(req, Ordering::Release);
        }
        core::hint::spin_loop();
    }
}

/// Ask the prober to read `SHOOT_VA` and return its result, 0 on timeout.
fn shoot_touch(req: u64) -> u64 {
    // Release: `SHOOT_VA` and the mapping change happen before the request.
    SHOOT_REQ.store(req, Ordering::Release);
    if !spin_until_ns(|| SHOOT_ACK.load(Ordering::Acquire) == req, 2_000_000_000) {
        return 0;
    }
    SHOOT_RESULT.load(Ordering::Relaxed)
}

/// End the prober and wait until it has stopped reading.
fn shoot_quit() {
    SHOOT_REQ.store(SHOOT_QUIT, Ordering::Release);
    let _ = spin_until_ns(
        || SHOOT_ACK.load(Ordering::Acquire) == SHOOT_QUIT,
        2_000_000_000,
    );
}

pub(crate) fn test_tlb_shootdown_remote() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    let Some(va) = alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let Some(pa) = alloc_frame() else {
        free_va(va, PAGE_SIZE);
        return Outcome::Fail("frame alloc");
    };
    // SAFETY: `paging_init::map_4k`'s contract; `pa` is a frame this test owns and `va` a KVA
    // page `alloc_va` reserved that nothing else maps; established here.
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        free_va(va, PAGE_SIZE);
        return Outcome::Fail("map");
    }
    // SAFETY: `va` is mapped writable to memory this test owns (the map above); established
    // here.
    unsafe { (va.as_u64() as *mut u64).write_volatile(0xD15EA5E) };

    SHOOT_VA.store(va.as_u64(), Ordering::Relaxed);
    SHOOT_REQ.store(0, Ordering::Relaxed);
    SHOOT_ACK.store(0, Ordering::Relaxed);
    let _prober = spawn_thread_on("shoot-probe", shoot_prober, ap);
    let r = shoot_touch(1);
    if r != 1 {
        shoot_quit();
        // SAFETY: `unmap_4k`'s contract; `va` is a test page, neither a stack nor code nor
        // heap, and the test does not touch it again until it frees or remaps it; established
        // here.
        let _ = unsafe { unmap_4k(va) };
        free_frame(pa);
        free_va(va, PAGE_SIZE);
        return if r == 0 {
            Outcome::Fail("AP probe did not answer")
        } else {
            Outcome::Fail("AP could not read mapped page")
        };
    }

    let before = crate::irq::ktest::shootdown_count();
    // SAFETY: `unmap_4k`'s contract; `va` is a test page, neither a stack nor code nor heap,
    // and the test does not touch it again until it frees or remaps it; established here.
    let _ = unsafe { unmap_4k(va) };
    if shoot_touch(2) != 2 {
        shoot_quit();
        // SAFETY: `paging_init::map_4k`'s contract; `pa` is a frame this test owns and `va` a
        // KVA page `alloc_va` reserved that nothing else maps; established here.
        let _ = unsafe { paging_init::map_4k(va, pa, heap_flags()) };
        free_frame(pa);
        free_va(va, PAGE_SIZE);
        return Outcome::Fail("AP did not fault after unmap");
    }
    if per_cpu_init::online_mask().count_ones() > 1
        && crate::irq::ktest::shootdown_count() <= before
    {
        shoot_quit();
        // SAFETY: `paging_init::map_4k`'s contract; `pa` is a frame this test owns and `va` a
        // KVA page `alloc_va` reserved that nothing else maps; established here.
        let _ = unsafe { paging_init::map_4k(va, pa, heap_flags()) };
        free_frame(pa);
        free_va(va, PAGE_SIZE);
        return Outcome::Fail("no shootdown IPI");
    }

    // SAFETY: `paging_init::map_4k`'s contract; `pa` is a frame this test owns and `va` a KVA
    // page `alloc_va` reserved that nothing else maps; established here.
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        shoot_quit();
        free_frame(pa);
        free_va(va, PAGE_SIZE);
        return Outcome::Fail("remap");
    }
    // SAFETY: `va` is mapped writable to memory this test owns (the map above); established
    // here.
    unsafe { (va.as_u64() as *mut u64).write_volatile(0xD15EA5E) };
    let ok = shoot_touch(3) == 1;
    shoot_quit();
    // SAFETY: `unmap_4k`'s contract; `va` is a test page, neither a stack nor code nor heap,
    // and the test does not touch it again until it frees or remaps it; established here.
    let _ = unsafe { unmap_4k(va) };
    free_frame(pa);
    free_va(va, PAGE_SIZE);
    if !ok {
        return Outcome::Fail("AP could not read after remap");
    }
    Outcome::Ok
}

static HAMMER_DONE: AtomicU32 = AtomicU32::new(0);

fn alloc_hammer() {
    let mut i = 0u32;
    while i < 128 {
        let b = Box::new([i; 16]);
        if b[0] != i {
            return;
        }
        i += 1;
    }
    HAMMER_DONE.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn test_alloc_stress_smp() -> Outcome {
    let mask = per_cpu_init::online_mask();
    let n = mask.count_ones();
    if n < 2 {
        return Outcome::Skip("no AP");
    }
    HAMMER_DONE.store(0, Ordering::SeqCst);
    let mut c = 1u32;
    while c < 64 {
        if mask & (1u64 << c) != 0 {
            let Ok(_) = thread_init::spawn_on("hammer", alloc_hammer, c) else {
                return Outcome::Fail("spawn");
            };
        }
        c += 1;
    }
    alloc_hammer();
    if !spin_until_ns(|| HAMMER_DONE.load(Ordering::SeqCst) >= n, 2_000_000_000) {
        crate::marker!(
            "vibeOS: ktest:   hammers {}",
            HAMMER_DONE.load(Ordering::SeqCst)
        );
        return Outcome::Fail("allocator stress hung");
    }
    Outcome::Ok
}

// ---------------------------------------------------------------------------
// frames_none_leaked (ROADMAP §10.3, F018)

const LEAK_LINE: &[u8] = b"vibeOS: meminfo: leaked 0 frames";

/// A `fmt::Write` sink that splits what `meminfo_to` writes into lines
/// and records whether one of them is [`LEAK_LINE`]. Lines longer than
/// its buffer are cut, which only makes them not match.
struct LineSeen {
    line: [u8; 96],
    len: usize,
    seen: bool,
}

impl fmt::Write for LineSeen {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                if &self.line[..self.len] == LEAK_LINE {
                    self.seen = true;
                }
                self.len = 0;
            } else if self.len < self.line.len() {
                self.line[self.len] = b;
                self.len += 1;
            }
        }
        Ok(())
    }
}

/// Every test before this one (the legacy list and the earlier suites)
/// freed each `Frames` it took, so none was dropped, and `meminfo` says
/// so on its `leaked` line.
pub(crate) fn frames_none_leaked() -> Outcome {
    let leaked = vibeos::pmm::leaked_frames();
    if leaked != 0 {
        return crate::fail_fmt!("{leaked} frames leaked by dropped Frames");
    }
    let mut sink = LineSeen {
        line: [0; 96],
        len: 0,
        seen: false,
    };
    diag::meminfo_to(&mut sink);
    if !sink.seen {
        return Outcome::Fail("meminfo printed no `leaked 0 frames` line");
    }
    Outcome::Ok
}

// ---------------------------------------------------------------------------
// current_mapper_holds_pt (ROADMAP §10.3, F018)

/// PT is held while a `current_mapper` guard lives and free once it drops.
/// Another CPU may take PT briefly after the drop, so the free check polls.
pub(crate) fn current_mapper_holds_pt() -> Outcome {
    let g = paging_init::current_mapper();
    let held = !pt_lock_free();
    let walks = g
        .translate(VirtAddr(current_mapper_holds_pt as *const () as u64))
        .is_some();
    drop(g);
    if !held {
        return Outcome::Fail("PT free while a MapperGuard lives");
    }
    if !walks {
        return Outcome::Fail("guard's mapper does not translate kernel text");
    }
    if !spin_until_ns(pt_lock_free, 100_000_000) {
        return Outcome::Fail("PT still held after drop");
    }
    Outcome::Ok
}

// ---------------------------------------------------------------------------
// vmap_32_frames_unmapped (ROADMAP §10.3, F018, F107)

/// Frames in the test's block: `MAX_UNMAP`, the most `vmap` takes.
const VMAP_ORDER: u8 = 5;

const VMAP_PAGES: u64 = 1 << VMAP_ORDER;

/// `unmap_shootdown` over `MAX_UNMAP` pages hits its assertion before it
/// takes PT, where it clamped silently before (ROADMAP §10.3, F107).
pub(crate) fn unmap_shootdown_over_max_asserts() -> Outcome {
    let n = MAX_UNMAP_PAGES + 1;
    let len = n as u64 * PAGE_SIZE;
    let Some(va) = alloc_va(len) else {
        return Outcome::Fail("kva alloc");
    };
    let if_on = crate::arch::current::interrupts_enabled();
    let nest = per_cpu_init::irq_nest();
    let hit = crate::arch::catch::catch_panic(|| kva_init::unmap_shootdown(va, n));
    free_va(va, len);
    if !hit {
        return Outcome::Fail("no assertion");
    }
    if crate::arch::current::interrupts_enabled() != if_on || per_cpu_init::irq_nest() != nest {
        return Outcome::Fail("irq_nest or IF changed");
    }
    Outcome::Ok
}

/// A 32-frame `vmap` maps the handle's own frames, and `vunmap` unmaps all
/// 32 pages and returns exactly the span it mapped to the KVA free list.
pub(crate) fn vmap_32_frames_unmapped() -> Outcome {
    // No other thread runs and no dead stack waits to be freed, so the KVA
    // use below moves only for this test's span.
    settle_threads();
    let Some(f) = alloc_frames_owned(VMAP_ORDER) else {
        return Outcome::Fail("no order-5 block");
    };
    let used0 = kva_init::stats().used;
    let v = match kva_init::vmap(f) {
        Ok(v) => v,
        Err(_) => return Outcome::Fail("vmap"),
    };
    let base = v.base().as_u64();
    if v.len() != VMAP_PAGES * PAGE_SIZE {
        free_frames_owned(kva_init::vunmap(v));
        return Outcome::Fail("len is not 32 pages");
    }
    if base < KVA_START || base.saturating_add(v.len()) > KVA_END {
        free_frames_owned(kva_init::vunmap(v));
        return Outcome::Fail("base outside the KVA window");
    }
    let mut i = 0u64;
    while i < VMAP_PAGES {
        // SAFETY: page `i` of the span `vmap` just mapped writable, which
        // nothing else uses until `vunmap` below; established here.
        unsafe { ((base + i * PAGE_SIZE) as *mut u64).write_volatile(i) };
        i += 1;
    }
    let frames = kva_init::vunmap(v);
    let mut still = 0u64;
    let mut wrong = 0u64;
    let mut i = 0u64;
    while i < VMAP_PAGES {
        if paging_init::translate(VirtAddr(base + i * PAGE_SIZE)).is_some() {
            still += 1;
        }
        let hhdm = paging_init::HHDM_BASE + frames.base() + i * PAGE_SIZE;
        // SAFETY: frame `i` of the block this test holds, read through the
        // physmap, which covers all RAM (DESIGN §4.1); established here.
        if unsafe { (hhdm as *const u64).read_volatile() } != i {
            wrong += 1;
        }
        i += 1;
    }
    let used1 = kva_init::stats().used;
    free_frames_owned(frames);
    if still != 0 {
        return crate::fail_fmt!("{still} of 32 pages still mapped after vunmap");
    }
    if wrong != 0 {
        return crate::fail_fmt!("{wrong} frames did not hold what their page wrote");
    }
    if used1 != used0 {
        return crate::fail_fmt!("kva used {used0} before vmap, {used1} after vunmap");
    }
    Outcome::Ok
}

// ------------------ hooks ------------------

// Test-only helpers over `kva_init`'s free list and `paging_init`'s
// tables (Q2).

/// Reserve `len` bytes of KVA with nothing mapped (Q2: no production
/// caller).
pub(crate) fn alloc_va(len: u64) -> Option<VirtAddr> {
    paging_init::with_pt(|_pt| kva_init::with_kva(|k| k.alloc(len)).map(VirtAddr))
}

/// Give back a range [`alloc_va`] reserved.
pub(crate) fn free_va(va: VirtAddr, len: u64) {
    kva_init::release_va(va, len);
}

/// Page-table pages the kernel mapper has taken from the buddy since boot,
/// the PML4 included. They stay in the kernel tables for good: a mapping
/// that reaches a 2 MiB span of KVA or heap no earlier mapping reached
/// takes one, and its unmap leaves it in place.
pub(crate) fn table_pages() -> usize {
    paging_init::TABLE_PAGES.load(Ordering::Relaxed)
}

/// Whether PT is free right now: held by no CPU, this one included. It
/// reads the lock rather than trying it, since a `try_lock` of a rank this
/// CPU holds fails the rank check.
pub(crate) fn pt_lock_free() -> bool {
    !paging_init::PT.is_locked()
}

/// Unmap one leaf, drop PT, then shootdown. Returns the frame.
///
/// # Safety
/// Caller is responsible for not unmapping a page the CPU is using
/// (stack, code, the heap it is currently allocating from, …).
pub unsafe fn unmap_4k(va: VirtAddr) -> Option<(PhysAddr, PageSize)> {
    // SAFETY: `unmap_4k_locked`'s contract; the caller will not use `va`
    // until the shootdown below (this fn's `# Safety` contract, established
    // here).
    let r = paging_init::with_pt(|pt| unsafe { paging_init::unmap_4k_locked(pt, va) });
    if r.is_some() {
        paging::tlb_shootdown_others(va);
    }
    r
}

/// A `kernel_tests` hook that fails every counted heap allocation after a
/// budget (ROADMAP §10.4, C-FAILAFTER). `heap_init`'s `KernelAlloc::alloc`
/// and `realloc` ask [`fail_after::refuse`] before they take the heap lock or grow the
/// heap, so a refused allocation maps no frame; `dealloc` never asks.
pub(crate) mod fail_after {
    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

    use vibeos::thread::ThreadId;

    use crate::arch::current::InterruptGuard;
    use crate::{per_cpu_init, thread_init};

    /// Which allocations the hook counts.
    #[derive(Clone, Copy)]
    pub(crate) enum Scope {
        /// Allocations on a process thread (pid != 0) whose
        /// `Tcb.syscall_count` is at least `from_syscall`: 1 counts from
        /// the thread's first syscall, 2 skips it.
        Processes { from_syscall: u64 },
        /// Allocations on one thread, a kernel thread included.
        Thread(ThreadId),
    }

    /// What the hook saw while armed: `counted` in-scope allocations, of
    /// which it refused the last `refused`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct Seen {
        pub counted: usize,
        pub refused: usize,
    }

    const KIND_PROCESSES: u8 = 0;
    const KIND_THREAD: u8 = 1;

    static ARMED: AtomicBool = AtomicBool::new(false);
    static KIND: AtomicU8 = AtomicU8::new(KIND_PROCESSES);
    /// `from_syscall`, or the thread id.
    static ARG: AtomicU64 = AtomicU64::new(0);
    static BUDGET: AtomicUsize = AtomicUsize::new(0);
    static COUNTED: AtomicUsize = AtomicUsize::new(0);
    static REFUSED: AtomicUsize = AtomicUsize::new(0);

    /// The first `budget` allocations in `scope` succeed; every later one
    /// returns null. One arm at a time.
    pub(crate) fn arm(budget: usize, scope: Scope) {
        assert!(!ARMED.load(Ordering::Acquire), "fail_after: armed twice");
        let (kind, arg) = match scope {
            Scope::Processes { from_syscall } => (KIND_PROCESSES, from_syscall),
            Scope::Thread(id) => (KIND_THREAD, u64::from(id.0)),
        };
        KIND.store(kind, Ordering::Relaxed);
        ARG.store(arg, Ordering::Relaxed);
        BUDGET.store(budget, Ordering::Relaxed);
        COUNTED.store(0, Ordering::Relaxed);
        REFUSED.store(0, Ordering::Relaxed);
        // Publishes the fields above to `refuse`'s Acquire load.
        ARMED.store(true, Ordering::Release);
    }

    /// Stop counting and return what the hook saw since [`arm`].
    pub(crate) fn disarm() -> Seen {
        ARMED.store(false, Ordering::Release);
        Seen {
            counted: COUNTED.load(Ordering::Acquire),
            refused: REFUSED.load(Ordering::Acquire),
        }
    }

    /// Whether this allocation is refused. Atomics only; never allocates.
    pub(crate) fn refuse() -> bool {
        if !ARMED.load(Ordering::Acquire) {
            return false;
        }
        // One thread's pid, id and count: no switch between the reads.
        let _irq = InterruptGuard::enter();
        if per_cpu_init::current_thread().is_null() {
            return false;
        }
        let arg = ARG.load(Ordering::Relaxed);
        let in_scope = match KIND.load(Ordering::Relaxed) {
            KIND_PROCESSES => {
                let t = per_cpu_init::current_thread();
                // SAFETY: invariant I9: the non-null current thread (checked
                // above) is this CPU's live TCB, which stays in `SCHED`;
                // established by `per_cpu_init::set_current_thread`.
                let count = unsafe { &(*t).syscall_count };
                // Relaxed: the count is a statistic (C-FAILAFTER).
                thread_init::current_pid() != 0 && count.load(Ordering::Relaxed) >= arg
            }
            _ => u64::from(thread_init::current_id().0) == arg,
        };
        if !in_scope {
            return false;
        }
        let n = COUNTED.fetch_add(1, Ordering::AcqRel);
        if n < BUDGET.load(Ordering::Relaxed) {
            return false;
        }
        REFUSED.fetch_add(1, Ordering::AcqRel);
        true
    }
}

/// `kernel_va0_faults`' probe result: 0 none yet, 1 `#PF` at CR2 0 on a
/// not-present supervisor read, 2 the read did not fault, 3 any other
/// fault. The CPU that ran it is in the upper 32 bits.
static VA0_RESULT: AtomicU64 = AtomicU64::new(0);

/// Read VA 0 under `catch_fault` with an asm load (a Rust null
/// dereference is UB) and classify the result.
#[cfg(target_arch = "x86_64")]
fn va0_probe() -> u64 {
    let fault = catch_fault(|| {
        // SAFETY: the load reads VA 0, which the identity teardown leaves
        // unmapped, and `catch_fault` recovers from the #PF it raises; the
        // asm writes only its scratch register; established here.
        unsafe {
            core::arch::asm!(
                "mov {tmp}, qword ptr [{va}]",
                va = in(reg) 0u64,
                tmp = out(reg) _,
                options(nostack, readonly, preserves_flags)
            );
        }
    });
    match fault {
        None => 2,
        // Error code: not present (bit 0), read (bit 1), supervisor (bit 2).
        Some(f) if f.cr2 == 0 && f.error & 0x7 == 0 => 1,
        Some(_) => 3,
    }
}

fn va0_entry() {
    // Pinned by `spawn_thread_on`, so the hint is this thread's CPU.
    let cpu = u64::from(thread_init::current_cpu());
    // Release: publishes the result to `kernel_va0_faults`.
    VA0_RESULT.store(cpu << 32 | va0_probe(), Ordering::Release);
}

/// ROADMAP §10.6: after `smp: done` the low identity window is gone, so a
/// kernel read of VA 0 faults on every online CPU. `catch_fault`'s one
/// jump buffer serves one CPU at a time, so the CPUs probe in turn.
pub(crate) fn kernel_va0_faults() -> Outcome {
    // The registry is pinned, so the hint is its CPU.
    let me = thread_init::current_cpu();
    let online = per_cpu_init::online_mask();
    for cpu in (0..64u32).filter(|c| online & (1u64 << c) != 0) {
        let r = if cpu == me {
            u64::from(cpu) << 32 | va0_probe()
        } else {
            VA0_RESULT.store(0, Ordering::Relaxed);
            spawn_thread_on("va0", va0_entry, cpu);
            if !spin_until_ns(|| VA0_RESULT.load(Ordering::Acquire) != 0, 2_000_000_000) {
                return crate::fail_fmt!("cpu {cpu}: probe did not run");
            }
            VA0_RESULT.load(Ordering::Acquire)
        };
        let (ran, what) = ((r >> 32) as u32, r & 0xFFFF_FFFF);
        if ran != cpu {
            return crate::fail_fmt!("cpu {cpu}: probe ran on cpu {ran}");
        }
        match what {
            1 => {}
            2 => return crate::fail_fmt!("cpu {cpu}: a kernel read of VA 0 did not fault"),
            _ => return crate::fail_fmt!("cpu {cpu}: VA 0 read took a fault other than #PF cr2=0"),
        }
    }
    Outcome::Ok
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("map_unmap", test_map_unmap),
    #[cfg(target_arch = "x86_64")]
    test("nx_enforcement", test_nx_enforcement),
    test("heap_box", test_heap_box),
    test("heap_reuse", test_heap_reuse),
    test("heap_align", test_heap_align),
    test("heap_growth", test_heap_growth),
    test("heap_oom", test_heap_oom),
    #[cfg(target_arch = "x86_64")]
    test("stack_guard", test_stack_guard),
    test("kva_roundtrip", test_kva_roundtrip),
    test("kva_deferred", test_kva_deferred),
    test("vmap", test_vmap),
    test("mmio_uc_flags", test_mmio_uc_flags),
    test("tlb_shootdown_remote", test_tlb_shootdown_remote),
    test("alloc_stress_smp", test_alloc_stress_smp),
    test("frames_none_leaked", frames_none_leaked),
    test("current_mapper_holds_pt", current_mapper_holds_pt),
    test("vmap_32_frames_unmapped", vmap_32_frames_unmapped),
    test(
        "unmap_shootdown_over_max_asserts",
        unmap_shootdown_over_max_asserts,
    ),
    test("kernel_va0_faults", kernel_va0_faults),
    test("ioremap_failure_returns_va", ioremap_failure_returns_va),
];

// ---------------------------------------------------------------------------
// ioremap_failure_returns_va

/// The ioremap window's cursor: the first VA it has not handed out.
fn window_next() -> u64 {
    paging_init::with_pt(|pt| pt.window().next())
}

/// A failed `ioremap` leaves none of its leaves mapped, and the window
/// hands out no VA that would refuse the next call the same way. A leaf
/// planted at the second page of the next reservation makes its
/// `map_range` map the first page and then refuse; the planted leaf, which
/// the call did not map, stays, and the cursor stays past it, so a second
/// `ioremap` beside the planted leaf succeeds. When the cursor went back
/// onto the planted leaf, every later `ioremap` that reached it failed.
pub(crate) fn ioremap_failure_returns_va() -> Outcome {
    let Some(frame) = alloc_frame() else {
        return Outcome::Fail("frame alloc");
    };
    let before = window_next();
    let planted = VirtAddr(before + PAGE_SIZE);
    // SAFETY: `paging_init::map_4k`'s contract; `frame` is this test's and
    // `planted` window VA the cursor has not reached, which nothing maps
    // until the unmap below; established here.
    if unsafe { paging_init::map_4k(planted, frame, heap_flags()) }.is_err() {
        free_frame(frame);
        return Outcome::Fail("plant");
    }
    // The LAPIC's page and the next: the map is refused at the second, and
    // nothing touches the first, so no UC alias of it is ever used.
    // SAFETY: `paging_init::ioremap`'s contract; `0xFEE0_0000` is the
    // LAPIC's MMIO, and the test never accesses the VA it would return;
    // established here.
    let got = unsafe { paging_init::ioremap(PhysAddr(0xFEE0_0000), 2 * PAGE_SIZE) };
    let after = window_next();
    let first = paging_init::translate(VirtAddr(before));
    // The same two pages again, with the planted leaf still in place.
    // SAFETY: as the call above; established here.
    let again = unsafe { paging_init::ioremap(PhysAddr(0xFEE0_0000), 2 * PAGE_SIZE) };
    let kept = paging_init::translate(planted).map(|(pa, _, _)| pa);
    // SAFETY: `unmap_4k`'s contract; nothing but this test reached
    // `planted`, and it does not touch it again; established here.
    let unplanted = unsafe { unmap_4k(planted) }.map(|(pa, _)| pa);
    free_frame(frame);
    if got.is_some() {
        return Outcome::Fail("ioremap over a mapped leaf succeeded");
    }
    if first.is_some() {
        return Outcome::Fail("first page still mapped");
    }
    if kept != Some(frame) || unplanted != Some(frame) {
        return Outcome::Fail("planted leaf not kept");
    }
    let Some(va) = again else {
        return Outcome::Fail("the next ioremap failed on the same leaf");
    };
    if after <= planted.as_u64() || va.as_u64() < after {
        return crate::fail_fmt!(
            "cursor {after:#x} after the failure, next VA {:#x}, planted leaf at {:#x}",
            va.as_u64(),
            planted.as_u64()
        );
    }
    Outcome::Ok
}
