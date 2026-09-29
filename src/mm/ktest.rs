//! In-guest tests for mm (kernel_tests only). Rows: the list in crate::ktest.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use vibeos::heap::HEAP_SIZE;
use vibeos::kva::{KVA_END, KVA_START, PAGE_SIZE};
use vibeos::paging::{PageFlags, PhysAddr, VirtAddr, heap_flags};

use crate::diag;
use crate::ktest::{
    Outcome, alloc_frame, alloc_frames_owned, catch_alloc_error, catch_fault, free_frame,
    free_frames, free_frames_owned, quiescent_free_frames, second_cpu, settle_threads,
    spawn_thread_on, spin_until_ns,
};
use crate::kva_init;
use crate::paging_init;
use crate::per_cpu_init;
use crate::thread_init;

// ------------------ tests ------------------

pub(crate) fn test_map_unmap() -> Outcome {
    let Some(va) = kva_init::alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let Some(pa) = alloc_frame() else {
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("frame alloc");
    };
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("map_4k");
    }
    unsafe { (va.as_u64() as *mut u64).write_volatile(0xAABB_CCDD_EEFF_0011) };
    let got = unsafe { (va.as_u64() as *const u64).read_volatile() };
    if got != 0xAABB_CCDD_EEFF_0011 {
        return Outcome::Fail("readback mismatch");
    }
    let Some((unmapped, _)) = (unsafe { paging_init::unmap_4k(va) }) else {
        return Outcome::Fail("unmap returned none");
    };
    if unmapped != pa {
        return Outcome::Fail("unmap phys mismatch");
    }
    free_frame(pa);
    kva_init::free_va(va, PAGE_SIZE);
    let fault = catch_fault(|| unsafe {
        (va.as_u64() as *mut u8).write_volatile(1);
    });
    match fault {
        Some(_) => Outcome::Ok,
        None => Outcome::Fail("access after unmap did not fault"),
    }
}

pub(crate) fn test_nx_enforcement() -> Outcome {
    let Some(va) = kva_init::alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let Some(pa) = alloc_frame() else {
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("frame alloc");
    };
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("map_4k");
    }
    unsafe { (va.as_u64() as *mut u8).write_volatile(0xC3) };
    let f: unsafe extern "C" fn() = unsafe { core::mem::transmute(va.as_u64()) };
    core::hint::black_box(f);
    let fault = catch_fault(|| unsafe { f() });
    let _ = unsafe { paging_init::unmap_4k(va) };
    free_frame(pa);
    kva_init::free_va(va, PAGE_SIZE);
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
        let p = unsafe { alloc::alloc::alloc(layout) };
        if p.is_null() {
            return Outcome::Fail("alloc null");
        }
        if !(p as usize).is_multiple_of(align) {
            unsafe { alloc::alloc::dealloc(p, layout) };
            return Outcome::Fail("alignment");
        }
        unsafe { p.write(0x5A) };
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
    let pad = unsafe { alloc::alloc::alloc(small) };
    let a = unsafe { alloc::alloc::alloc(layout) };
    let keep = unsafe { alloc::alloc::alloc(layout) };
    if pad.is_null() || a.is_null() || keep.is_null() {
        return Outcome::Fail("setup alloc");
    }
    unsafe { alloc::alloc::dealloc(a, layout) };
    let b = unsafe { alloc::alloc::alloc(layout) };
    if b.is_null() {
        return Outcome::Fail("second alloc");
    }
    // Compare as usize through black_box: LLVM treats GlobalAlloc like
    // malloc and will fold `a == b` after free at opt-level 1.
    let reused = core::hint::black_box(a as usize) == core::hint::black_box(b as usize);
    if !reused {
        crate::marker!("vibeOS: ktest:   reuse pad={pad:p} a={a:p} keep={keep:p} b={b:p}");
        unsafe {
            alloc::alloc::dealloc(b, layout);
            alloc::alloc::dealloc(keep, layout);
            alloc::alloc::dealloc(pad, small);
        };
        return Outcome::Fail("did not reuse freed block");
    }
    let c = unsafe { alloc::alloc::realloc(b, layout, 32) };
    let same = core::hint::black_box(c as usize) == core::hint::black_box(b as usize);
    if !c.is_null() {
        unsafe { alloc::alloc::dealloc(c, Layout::from_size_align(32, 8).unwrap()) };
    }
    unsafe { alloc::alloc::dealloc(keep, layout) };
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

pub(crate) fn test_stack_guard() -> Outcome {
    let Ok(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    unsafe { (stack.base().as_u64() as *mut u64).write_volatile(0x1111_2222) };
    let got = unsafe { (stack.base().as_u64() as *const u64).read_volatile() };
    if got != 0x1111_2222 {
        kva_init::free_stack(stack);
        return Outcome::Fail("mapped stack not writable");
    }
    let guard = stack.guard().as_u64();
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
    thread_init::testing::park_on_local_list(stack);
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
    unsafe { (va.as_u64() as *mut u64).write_volatile(0x100) };
    unsafe { ((va.as_u64() + PAGE_SIZE) as *mut u64).write_volatile(0x200) };
    let ga = unsafe { (va.as_u64() as *const u64).read_volatile() };
    let gb = unsafe { ((va.as_u64() + PAGE_SIZE) as *const u64).read_volatile() };
    free_frames_owned(kva_init::vunmap(v));
    if ga != 0x100 || gb != 0x200 {
        return Outcome::Fail("vmap readback");
    }
    Outcome::Ok
}

pub(crate) fn test_mmio_uc_flags() -> Outcome {
    // LAPIC (0xFEE0_0000) sits above QEMU's 128 MiB map_end, so the
    // generic patch API is still proven on a leaf we know exists:
    // 2 MiB, inside the identity rest / physmap. ACPI's real bases
    // are checked by `acpi_discovery`.
    let phys = PhysAddr(0x0020_0000);
    if unsafe { paging_init::patch_physmap_uc(phys, 4096) }.is_err() {
        return Outcome::Fail("patch_physmap_uc");
    }
    let va = VirtAddr(paging_init::HHDM_BASE + phys.as_u64());
    let Some((_, _, flags)) = paging_init::translate(va) else {
        return Outcome::Fail("translate");
    };
    if !flags.contains(PageFlags::PCD | PageFlags::PWT) {
        return Outcome::Fail("PCD/PWT not set on physmap leaf");
    }
    Outcome::Ok
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
    let Some(va) = kva_init::alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let Some(pa) = alloc_frame() else {
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("frame alloc");
    };
    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("map");
    }
    unsafe { (va.as_u64() as *mut u64).write_volatile(0xD15EA5E) };

    SHOOT_VA.store(va.as_u64(), Ordering::Relaxed);
    SHOOT_REQ.store(0, Ordering::Relaxed);
    SHOOT_ACK.store(0, Ordering::Relaxed);
    let _prober = spawn_thread_on("shoot-probe", shoot_prober, ap);
    let r = shoot_touch(1);
    if r != 1 {
        shoot_quit();
        let _ = unsafe { paging_init::unmap_4k(va) };
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return if r == 0 {
            Outcome::Fail("AP probe did not answer")
        } else {
            Outcome::Fail("AP could not read mapped page")
        };
    }

    let before = crate::irq::ktest::shootdown_count();
    let _ = unsafe { paging_init::unmap_4k(va) };
    if shoot_touch(2) != 2 {
        shoot_quit();
        let _ = unsafe { paging_init::map_4k(va, pa, heap_flags()) };
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("AP did not fault after unmap");
    }
    if per_cpu_init::online_mask().count_ones() > 1
        && crate::irq::ktest::shootdown_count() <= before
    {
        shoot_quit();
        let _ = unsafe { paging_init::map_4k(va, pa, heap_flags()) };
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("no shootdown IPI");
    }

    if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
        shoot_quit();
        free_frame(pa);
        kva_init::free_va(va, PAGE_SIZE);
        return Outcome::Fail("remap");
    }
    unsafe { (va.as_u64() as *mut u64).write_volatile(0xD15EA5E) };
    let ok = shoot_touch(3) == 1;
    shoot_quit();
    let _ = unsafe { paging_init::unmap_4k(va) };
    free_frame(pa);
    kva_init::free_va(va, PAGE_SIZE);
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
    let held = !paging_init::pt_lock_free();
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
    if !spin_until_ns(paging_init::pt_lock_free, 100_000_000) {
        return Outcome::Fail("PT still held after drop");
    }
    Outcome::Ok
}

// ---------------------------------------------------------------------------
// vmap_32_frames_unmapped (ROADMAP §10.3, F018, F107)

/// Frames in the test's block: `MAX_UNMAP`, the most `vmap` takes.
const VMAP_ORDER: u8 = 5;

const VMAP_PAGES: u64 = 1 << VMAP_ORDER;

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
