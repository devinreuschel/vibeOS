//! In-guest test registry. DESIGN §8.2.
//!
//! Built only with `--features kernel_tests`. After normal init this
//! module installs a tiny IDT (enough to catch #PF), runs the registry,
//! prints the serial protocol, and exits QEMU through `isa-debug-exit`.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::arch::asm;
use core::arch::global_asm;
use core::fmt::Write;

use vibeos::heap::HEAP_SIZE;
use vibeos::kva::PAGE_SIZE;
use vibeos::paging::{heap_flags, PageFlags, PhysAddr, VirtAddr};

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
    Skip(&'static str),
}

type TestFn = fn() -> Outcome;

const TESTS: &[(&str, TestFn)] = &[
    ("map_unmap", test_map_unmap),
    ("nx_enforcement", test_nx_enforcement),
    ("heap_box", test_heap_box),
    ("heap_growth", test_heap_growth),
    ("heap_align", test_heap_align),
    ("heap_reuse", test_heap_reuse),
    ("heap_oom", test_heap_oom),
    ("stack_guard", test_stack_guard),
    ("kva_roundtrip", test_kva_roundtrip),
    ("kva_deferred", test_kva_deferred),
    ("vmap", test_vmap),
    ("mmio_uc_flags", test_mmio_uc_flags),
];

pub fn run() -> ! {
    // IRQs off for the whole registry: the tiny IDT is only #PF-aware,
    // and a stray PIC vector would look like a hang.
    let _cli = x86::InterruptGuard::enter();
    install_idt();
    serial::line("vibeOS: ktest: begin");
    let mut failed = false;
    for &(name, f) in TESTS {
        match f() {
            Outcome::Ok => {
                let _ = writeln!(Serial, "vibeOS: ktest: ok {name}");
            }
            Outcome::Fail(why) => {
                let _ = writeln!(Serial, "vibeOS: ktest: FAIL {name}");
                let _ = writeln!(Serial, "vibeOS: ktest:   {why}");
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

// ------------------ scoped #PF / alloc-error catcher ------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct JmpBuf {
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rsp: u64,
    rip: u64,
}

#[derive(Clone, Copy)]
struct Fault {
    cr2: u64,
    error: u64,
}

enum CatchState {
    Off,
    Fault,
    Alloc,
}

static mut CATCH: CatchState = CatchState::Off;
static mut LAST_FAULT: Fault = Fault { cr2: 0, error: 0 };
static mut JMPBUF: JmpBuf = JmpBuf {
    rbx: 0,
    rbp: 0,
    r12: 0,
    r13: 0,
    r14: 0,
    r15: 0,
    rsp: 0,
    rip: 0,
};

unsafe extern "C" {
    #[ffi_returns_twice]
    fn vibeos_setjmp(buf: *mut JmpBuf) -> i32;
    fn vibeos_longjmp(buf: *mut JmpBuf, val: i32) -> !;
    fn ktest_pf_stub();
    fn ktest_unhandled_stub();
}

// AT&T: `global_asm!` default on x86_64. JMPBUF is a static so longjmp
// never reads a stack copy that dies when we restore RSP.
global_asm!(
    r#"
    .pushsection .text

    .global vibeos_setjmp
    vibeos_setjmp:
        movq %rbx, 0x00(%rdi)
        movq %rbp, 0x08(%rdi)
        movq %r12, 0x10(%rdi)
        movq %r13, 0x18(%rdi)
        movq %r14, 0x20(%rdi)
        movq %r15, 0x28(%rdi)
        leaq 8(%rsp), %rax
        movq %rax, 0x30(%rdi)
        movq (%rsp), %rax
        movq %rax, 0x38(%rdi)
        xorl %eax, %eax
        ret

    .global vibeos_longjmp
    vibeos_longjmp:
        movq 0x00(%rdi), %rbx
        movq 0x08(%rdi), %rbp
        movq 0x10(%rdi), %r12
        movq 0x18(%rdi), %r13
        movq 0x20(%rdi), %r14
        movq 0x28(%rdi), %r15
        movq 0x30(%rdi), %rsp
        movl %esi, %eax
        testl %eax, %eax
        jnz 1f
        movl $1, %eax
    1:
        jmpq *0x38(%rdi)

    .global ktest_pf_stub
    ktest_pf_stub:
        pushq %rdi
        pushq %rsi
        movq %cr2, %rdi
        movq 16(%rsp), %rsi
        call ktest_page_fault
        popq %rsi
        popq %rdi
        addq $8, %rsp
        iretq

    .global ktest_unhandled_stub
    ktest_unhandled_stub:
        call ktest_unhandled
    2:  hlt
        jmp 2b

    .popsection
    "#
);

#[unsafe(no_mangle)]
extern "C" fn ktest_page_fault(cr2: u64, error: u64) {
    unsafe {
        match CATCH {
            CatchState::Fault => {
                LAST_FAULT = Fault { cr2, error };
                CATCH = CatchState::Off;
                vibeos_longjmp(core::ptr::addr_of_mut!(JMPBUF), 1);
            }
            CatchState::Off | CatchState::Alloc => {}
        }
    }
    let _ = writeln!(
        Serial,
        "vibeOS: ktest: uncaught #PF cr2={cr2:#x} err={error:#x}"
    );
    x86::halt();
}

#[unsafe(no_mangle)]
extern "C" fn ktest_unhandled() {
    serial::line("vibeOS: ktest: unhandled exception");
    x86::halt();
}

/// Called from `#[alloc_error_handler]`. Longjmps if a test is catching.
pub fn on_alloc_error(layout: Layout) {
    unsafe {
        if let CatchState::Alloc = CATCH {
            CATCH = CatchState::Off;
            vibeos_longjmp(core::ptr::addr_of_mut!(JMPBUF), 1);
        }
    }
    let _ = layout;
}

fn catch_fault<F: FnOnce()>(f: F) -> Option<Fault> {
    unsafe {
        if vibeos_setjmp(core::ptr::addr_of_mut!(JMPBUF)) != 0 {
            CATCH = CatchState::Off;
            return Some(LAST_FAULT);
        }
        CATCH = CatchState::Fault;
        f();
        CATCH = CatchState::Off;
        None
    }
}

fn catch_alloc_error<F: FnOnce()>(f: F) -> bool {
    unsafe {
        if vibeos_setjmp(core::ptr::addr_of_mut!(JMPBUF)) != 0 {
            CATCH = CatchState::Off;
            return true;
        }
        CATCH = CatchState::Alloc;
        f();
        CATCH = CatchState::Off;
        false
    }
}

#[repr(C, packed)]
struct IdtEntry {
    off_lo: u16,
    selector: u16,
    ist_type: u16,
    off_mid: u16,
    off_hi: u32,
    zero: u32,
}

impl IdtEntry {
    const EMPTY: Self = Self {
        off_lo: 0,
        selector: 0,
        ist_type: 0,
        off_mid: 0,
        off_hi: 0,
        zero: 0,
    };

    fn gate(handler: u64, cs: u16) -> Self {
        Self {
            off_lo: handler as u16,
            selector: cs,
            ist_type: 0x8E00,
            off_mid: (handler >> 16) as u16,
            off_hi: (handler >> 32) as u32,
            zero: 0,
        }
    }
}

#[repr(C, align(16))]
struct IdtTable([IdtEntry; 256]);

static mut IDT: IdtTable = IdtTable([IdtEntry::EMPTY; 256]);

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

fn install_idt() {
    let cs = x86::read_cs();
    let unhandled = ktest_unhandled_stub as u64;
    let pf = ktest_pf_stub as u64;
    unsafe {
        for slot in IDT.0.iter_mut() {
            *slot = IdtEntry::gate(unhandled, cs);
        }
        IDT.0[14] = IdtEntry::gate(pf, cs);
        let idtr = Idtr {
            limit: (core::mem::size_of::<IdtTable>() - 1) as u16,
            base: core::ptr::addr_of!(IDT) as u64,
        };
        asm!(
            "lidt [{}]",
            in(reg) core::ptr::addr_of!(idtr),
            options(readonly, nostack, preserves_flags)
        );
    }
}

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
    let Ok(layout) = Layout::from_size_align(64, 8) else {
        return Outcome::Fail("layout");
    };
    let a = unsafe { alloc::alloc::alloc(layout) };
    if a.is_null() {
        return Outcome::Fail("first alloc");
    }
    unsafe { alloc::alloc::dealloc(a, layout) };
    let b = unsafe { alloc::alloc::alloc(layout) };
    if b.is_null() {
        return Outcome::Fail("second alloc");
    }
    let reuse = a == b;
    unsafe { alloc::alloc::dealloc(b, layout) };
    if reuse {
        Outcome::Ok
    } else {
        Outcome::Fail("did not reuse freed block")
    }
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
    // Default LAPIC base sits in the physmap (well under the 8 GiB cap).
    // Patching that 2 MiB leaf UC is the §1.3 read-back; QEMU does not
    // care about cache attributes, so this is safe on the boot path.
    let phys = PhysAddr(0xFEE0_0000);
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
