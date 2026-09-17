//! In-guest test registry. DESIGN §8.2.
//!
//! Built only with `--features kernel_tests`. After normal init this
//! module runs the registry over the real IDT, prints the serial protocol,
//! and exits QEMU through `isa-debug-exit`.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::arch::global_asm;
use core::fmt::Write;

use vibeos::desc::{IstSlot, KERNEL_CS, TSS_SEL};
use vibeos::heap::HEAP_SIZE;
use vibeos::kva::PAGE_SIZE;
use vibeos::paging::{heap_flags, PageFlags, PhysAddr, VirtAddr};
use vibeos::vectors;

use crate::acpi_init;
use crate::arch;
use crate::kva_init;
use crate::paging_init;
use crate::pmm_init;
use crate::serial::{self, Serial};
use crate::x86;

const ISA_DEBUG_EXIT: u16 = 0xF4;
const EXIT_PASS: u32 = 0x10;
const EXIT_FAIL: u32 = 0x11;

#[derive(Clone, Copy)]
enum Outcome {
    Ok,
    Fail(&'static str),
    #[allow(dead_code)] // protocol is first-class; no skips in this slice
    Skip(&'static str),
}

type TestFn = fn() -> Outcome;

const TESTS: &[(&str, TestFn)] = &[
    ("map_unmap", test_map_unmap),
    ("nx_enforcement", test_nx_enforcement),
    ("heap_box", test_heap_box),
    ("heap_reuse", test_heap_reuse),
    ("heap_align", test_heap_align),
    ("heap_growth", test_heap_growth),
    ("heap_oom", test_heap_oom),
    ("stack_guard", test_stack_guard),
    ("kva_roundtrip", test_kva_roundtrip),
    ("kva_deferred", test_kva_deferred),
    ("vmap", test_vmap),
    ("mmio_uc_flags", test_mmio_uc_flags),
    ("acpi_discovery", test_acpi_discovery),
    ("gdt_selectors", test_gdt_selectors),
    ("int3_roundtrip", test_int3_roundtrip),
    ("scoped_pf", test_scoped_pf),
    ("gp_catch", test_gp_catch),
    ("df_on_ist", test_df_on_ist),
];

pub fn run() -> ! {
    // IRQs off: PIC is masked, but page-table walks should not race a
    // stray spurious line.
    let _cli = x86::InterruptGuard::enter();
    serial::line("vibeOS: ktest: begin");
    let mut failed = false;
    for &(name, f) in TESTS {
        match f() {
            Outcome::Ok => {
                let _ = writeln!(Serial, "vibeOS: ktest: ok {name}");
            }
            Outcome::Fail(why) => {
                // Same shape as skip: reason on the protocol line so
                // check_ktest_output (which raises on that line alone)
                // is enough to diagnose (DESIGN §8.2).
                let _ = writeln!(Serial, "vibeOS: ktest: FAIL {name}: {why}");
                failed = true;
            }
            Outcome::Skip(reason) => {
                let _ = writeln!(Serial, "vibeOS: ktest: skip {name}: {reason}");
            }
        }
    }
    serial::line("vibeOS: ktest: end");
    qemu_exit(if failed { EXIT_FAIL } else { EXIT_PASS });
}

fn qemu_exit(code: u32) -> ! {
    unsafe { x86::outl(ISA_DEBUG_EXIT, code) };
    x86::halt();
}

fn free_frames() -> usize {
    unsafe { pmm_init::with_buddy(|b| b.stats().free_frames) }
}

fn alloc_frame() -> Option<PhysAddr> {
    unsafe { pmm_init::with_buddy(|b| b.allocate_frame()) }.map(PhysAddr)
}

fn free_frame(pa: PhysAddr) {
    unsafe { pmm_init::with_buddy(|b| b.deallocate_frame(pa.as_u64())) };
}

struct Fault {
    cr2: u64,
    error: u64,
}

fn catch_fault<F: FnOnce()>(f: F) -> Option<Fault> {
    arch::catch::catch(vectors::PF, f).map(|c| Fault {
        cr2: c.cr2,
        error: c.error,
    })
}

fn catch_alloc_error<F: FnOnce()>(f: F) -> bool {
    arch::catch::catch_alloc(f)
}

unsafe extern "C" {
    fn vibeos_write_u8_1(addr: u64);
    fn vibeos_fault_on_bad_stack(rsp: u64) -> !;
}

// Known-length store (`C6 07 01`, 3 bytes) for the skip-RIP catcher.
// `ud2` on an unmapped RSP forces #UD delivery to fail into #DF on IST1.
global_asm!(
    r#"
    .pushsection .text
    .global vibeos_write_u8_1
    vibeos_write_u8_1:
        mov byte ptr [rdi], 1
        ret
    .global vibeos_fault_on_bad_stack
    vibeos_fault_on_bad_stack:
        mov rsp, rdi
        ud2
    .popsection
    "#
);

const WRITE_U8_1_LEN: u8 = 3;

// ------------------ tests ------------------

fn test_map_unmap() -> Outcome {
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

fn test_nx_enforcement() -> Outcome {
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
        let _ = writeln!(
            Serial,
            "vibeOS: ktest:   nx err={:#x} cr2={:#x}",
            fault.error, fault.cr2
        );
        return Outcome::Fail("PF was not instruction-fetch");
    }
    Outcome::Ok
}

fn test_heap_box() -> Outcome {
    let b = Box::new(0xDEAD_BEEFu64);
    if *b != 0xDEAD_BEEF {
        return Outcome::Fail("box payload");
    }
    drop(b);
    Outcome::Ok
}

fn test_heap_growth() -> Outcome {
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

fn test_heap_align() -> Outcome {
    let mut align = 1usize;
    while align <= 4096 {
        let Ok(layout) = Layout::from_size_align(align, align) else {
            return Outcome::Fail("layout");
        };
        let p = unsafe { alloc::alloc::alloc(layout) };
        if p.is_null() {
            return Outcome::Fail("alloc null");
        }
        if p as usize % align != 0 {
            unsafe { alloc::alloc::dealloc(p, layout) };
            return Outcome::Fail("alignment");
        }
        unsafe { p.write(0x5A) };
        unsafe { alloc::alloc::dealloc(p, layout) };
        align *= 2;
    }
    Outcome::Ok
}

fn test_heap_reuse() -> Outcome {
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
        let _ = writeln!(
            Serial,
            "vibeOS: ktest:   reuse pad={pad:p} a={a:p} keep={keep:p} b={b:p}"
        );
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

fn test_heap_oom() -> Outcome {
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

fn test_stack_guard() -> Outcome {
    let Some(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    unsafe {
        (stack.mapped_base().as_u64() as *mut u64).write_volatile(0x1111_2222)
    };
    let got = unsafe { (stack.mapped_base().as_u64() as *const u64).read_volatile() };
    if got != 0x1111_2222 {
        kva_init::free_stack(stack);
        return Outcome::Fail("mapped stack not writable");
    }
    let guard = stack.guard.as_u64();
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

fn test_kva_roundtrip() -> Outcome {
    let before = free_frames();
    let Some(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    let mid = free_frames();
    if mid + 4 != before {
        kva_init::free_stack(stack);
        let _ = writeln!(Serial, "vibeOS: ktest:   before={before} mid={mid}");
        return Outcome::Fail("stack did not take 4 frames");
    }
    kva_init::free_stack(stack);
    let after = free_frames();
    if after != before {
        let _ = writeln!(Serial, "vibeOS: ktest:   before={before} after={after}");
        return Outcome::Fail("free did not restore frame count");
    }
    Outcome::Ok
}

fn test_kva_deferred() -> Outcome {
    let before = free_frames();
    let Some(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    let mid = free_frames();
    kva_init::defer_free(stack);
    if free_frames() != mid {
        kva_init::drain_deferred();
        return Outcome::Fail("defer freed too early");
    }
    kva_init::drain_deferred();
    let after = free_frames();
    if after != before {
        let _ = writeln!(Serial, "vibeOS: ktest:   before={before} after={after}");
        return Outcome::Fail("drain did not free");
    }
    Outcome::Ok
}

fn test_vmap() -> Outcome {
    let Some(a) = alloc_frame() else {
        return Outcome::Fail("frame a");
    };
    let Some(b) = alloc_frame() else {
        free_frame(a);
        return Outcome::Fail("frame b");
    };
    let Some(va) = kva_init::vmap(&[a, b]) else {
        free_frame(a);
        free_frame(b);
        return Outcome::Fail("vmap");
    };
    unsafe { (va.as_u64() as *mut u64).write_volatile(0x100) };
    unsafe { ((va.as_u64() + PAGE_SIZE) as *mut u64).write_volatile(0x200) };
    let ga = unsafe { (va.as_u64() as *const u64).read_volatile() };
    let gb = unsafe { ((va.as_u64() + PAGE_SIZE) as *const u64).read_volatile() };
    kva_init::vunmap(va, 2);
    free_frame(a);
    free_frame(b);
    if ga != 0x100 || gb != 0x200 {
        return Outcome::Fail("vmap readback");
    }
    Outcome::Ok
}

fn test_mmio_uc_flags() -> Outcome {
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

fn leaf_is_uc(phys: u64) -> bool {
    if phys == 0 {
        return false;
    }
    let va = VirtAddr(paging_init::HHDM_BASE.wrapping_add(phys));
    match paging_init::translate(va) {
        Some((_, _, flags)) => flags.contains(PageFlags::PCD | PageFlags::PWT),
        None => false,
    }
}

fn test_acpi_discovery() -> Outcome {
    let Some(info) = acpi_init::info() else {
        return Outcome::Fail("no acpi info");
    };
    if info.table_count == 0 {
        return Outcome::Fail("zero tables");
    }
    if info.cpu_count() == 0 {
        return Outcome::Fail("no enabled cpus");
    }
    if info.ioapic_count() == 0 {
        return Outcome::Fail("no ioapic");
    }
    if !info.hpet_present() {
        return Outcome::Fail("no hpet");
    }
    if !acpi_init::mmio_uc_patched() {
        return Outcome::Fail("mmio uc not patched");
    }
    let Some(madt) = info.madt.as_ref() else {
        return Outcome::Fail("no madt");
    };
    if !leaf_is_uc(madt.lapic_base) {
        return Outcome::Fail("lapic not uc");
    }
    for i in 0..madt.ioapic_count {
        if !leaf_is_uc(madt.ioapics[i].addr as u64) {
            return Outcome::Fail("ioapic not uc");
        }
    }
    let Some(hpet) = info.hpet else {
        return Outcome::Fail("no hpet");
    };
    if !leaf_is_uc(hpet.base) {
        return Outcome::Fail("hpet not uc");
    }
    if hpet.period_fs == 0 {
        return Outcome::Fail("hpet period unread");
    }
    Outcome::Ok
}

fn test_gdt_selectors() -> Outcome {
    if x86::read_cs() != KERNEL_CS {
        return Outcome::Fail("cs not kernel code");
    }
    if x86::read_tr() != TSS_SEL {
        return Outcome::Fail("tr not tss");
    }
    Outcome::Ok
}

fn test_int3_roundtrip() -> Outcome {
    unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
    Outcome::Ok
}

fn test_scoped_pf() -> Outcome {
    let Some(va) = kva_init::alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    let caught = arch::catch::catch_skip(vectors::PF, WRITE_U8_1_LEN, || unsafe {
        vibeos_write_u8_1(va.as_u64());
    });
    kva_init::free_va(va, PAGE_SIZE);
    let Some(c) = caught else {
        return Outcome::Fail("store did not fault");
    };
    if c.vector != vectors::PF {
        return Outcome::Fail("wrong vector");
    }
    if (c.cr2 & !0xFFF) != (va.as_u64() & !0xFFF) {
        return Outcome::Fail("cr2 not the unmapped page");
    }
    Outcome::Ok
}

fn test_gp_catch() -> Outcome {
    let caught = arch::catch::catch(vectors::GP, || unsafe {
        core::arch::asm!(
            "mov ds, {0:x}",
            in(reg) 0x0Bu16,
            options(nostack, preserves_flags)
        );
    });
    match caught {
        Some(c)
            if c.vector == vectors::GP
                && c.frame.cs == KERNEL_CS as u64
                && c.frame.rip != 0 =>
        {
            Outcome::Ok
        }
        Some(_) => Outcome::Fail("wrong vector or frame"),
        None => Outcome::Fail("no gp"),
    }
}

fn test_df_on_ist() -> Outcome {
    let Some(stack) = kva_init::alloc_guarded_stack(1) else {
        return Outcome::Fail("guarded stack");
    };
    let poison = stack.guard.as_u64() + 0x800;
    let caught = arch::catch::catch(vectors::DF, || unsafe {
        vibeos_fault_on_bad_stack(poison);
    });
    kva_init::free_stack(stack);
    let Some(c) = caught else {
        return Outcome::Fail("did not reach df handler");
    };
    let (lo, hi) = arch::gdt::ist_span(IstSlot::DoubleFault);
    if c.handler_rsp >= lo && c.handler_rsp < hi {
        Outcome::Ok
    } else {
        let _ = writeln!(
            Serial,
            "vibeOS: ktest:   rsp={:#x} lo={:#x} hi={:#x}",
            c.handler_rsp, lo, hi
        );
        Outcome::Fail("handler rsp not on ist1")
    }
}
