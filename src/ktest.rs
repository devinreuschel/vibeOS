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
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::ptr;

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

#[derive(Clone, Copy)]
enum CatchState {
    Off,
    Fault,
    Alloc,
}

/// Interior mutability so we never form `&mut` to a `static mut` (2024 deny).
struct Cell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Cell<T> {}
impl<T> Cell<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }
    fn ptr(&self) -> *mut T {
        self.0.get()
    }
}

static CATCH: Cell<CatchState> = Cell::new(CatchState::Off);
static LAST_FAULT: Cell<Fault> = Cell::new(Fault { cr2: 0, error: 0 });
static THUNK_DATA: Cell<*mut u8> = Cell::new(core::ptr::null_mut());
static THUNK_CALL: Cell<Option<unsafe fn(*mut u8)>> = Cell::new(None);

unsafe extern "C" {
    fn vibeos_catch() -> i32;
    fn vibeos_longjmp(buf: *mut JmpBuf, val: i32) -> !;
    fn ktest_pf_stub();
    fn ktest_unhandled_stub();
    static mut vibeos_jmpbuf: JmpBuf;
}

// Catch lives in asm so LLVM never sees setjmp (ffi_returns_twice is gone).
// `vibeos_catch` setjmps, calls `ktest_run_thunk`, returns 0; longjmp
// returns 1 through the same asm frame, which then rets into Rust.
global_asm!(
    r#"
    .pushsection .bss
    .align 8
    .global vibeos_jmpbuf
    vibeos_jmpbuf:
        .skip 64
    .popsection

    .pushsection .text

    .global vibeos_setjmp
    vibeos_setjmp:
        mov [rdi + 0x00], rbx
        mov [rdi + 0x08], rbp
        mov [rdi + 0x10], r12
        mov [rdi + 0x18], r13
        mov [rdi + 0x20], r14
        mov [rdi + 0x28], r15
        lea rax, [rsp + 8]
        mov [rdi + 0x30], rax
        mov rax, [rsp]
        mov [rdi + 0x38], rax
        xor eax, eax
        ret

    .global vibeos_longjmp
    vibeos_longjmp:
        mov rbx, [rdi + 0x00]
        mov rbp, [rdi + 0x08]
        mov r12, [rdi + 0x10]
        mov r13, [rdi + 0x18]
        mov r14, [rdi + 0x20]
        mov r15, [rdi + 0x28]
        mov rsp, [rdi + 0x30]
        mov eax, esi
        test eax, eax
        jnz 1f
        mov eax, 1
    1:
        jmp [rdi + 0x38]

    .global vibeos_catch
    vibeos_catch:
        push rbp
        mov rbp, rsp
        lea rdi, [rip + vibeos_jmpbuf]
        call vibeos_setjmp
        test eax, eax
        jnz 1f
        call ktest_run_thunk
        xor eax, eax
        pop rbp
        ret
    1:
        mov eax, 1
        pop rbp
        ret

    .global ktest_pf_stub
    ktest_pf_stub:
        push rdi
        push rsi
        mov rdi, cr2
        mov rsi, [rsp + 16]
        call ktest_page_fault
        pop rsi
        pop rdi
        add rsp, 8
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
        match ptr::read(CATCH.ptr()) {
            CatchState::Fault => {
                ptr::write(LAST_FAULT.ptr(), Fault { cr2, error });
                ptr::write(CATCH.ptr(), CatchState::Off);
                vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1);
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
        if let CatchState::Alloc = ptr::read(CATCH.ptr()) {
            ptr::write(CATCH.ptr(), CatchState::Off);
            vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1);
        }
    }
    let _ = layout;
}

unsafe fn invoke<F: FnOnce()>(p: *mut u8) {
    let slot = unsafe { &mut *(p as *mut Option<F>) };
    slot.take().unwrap()();
}

fn catch_with<F: FnOnce()>(kind: CatchState, f: F) -> i32 {
    let mut slot = Some(f);
    unsafe {
        ptr::write(THUNK_DATA.ptr(), (&raw mut slot).cast());
        ptr::write(THUNK_CALL.ptr(), Some(invoke::<F>));
        ptr::write(CATCH.ptr(), kind);
        let rc = vibeos_catch();
        ptr::write(CATCH.ptr(), CatchState::Off);
        ptr::write(THUNK_CALL.ptr(), None);
        rc
    }
}

#[unsafe(no_mangle)]
extern "C" fn ktest_run_thunk() {
    unsafe {
        let call = ptr::read(THUNK_CALL.ptr());
        ptr::write(THUNK_CALL.ptr(), None);
        if let Some(call) = call {
            let data = ptr::read(THUNK_DATA.ptr());
            ptr::write(THUNK_DATA.ptr(), core::ptr::null_mut());
            call(data);
        }
    }
}

fn catch_fault<F: FnOnce()>(f: F) -> Option<Fault> {
    if catch_with(CatchState::Fault, f) != 0 {
        Some(unsafe { ptr::read(LAST_FAULT.ptr()) })
    } else {
        None
    }
}

fn catch_alloc_error<F: FnOnce()>(f: F) -> bool {
    catch_with(CatchState::Alloc, f) != 0
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
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

static IDT: Cell<IdtTable> = Cell::new(IdtTable([IdtEntry::EMPTY; 256]));

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

fn install_idt() {
    let cs = x86::read_cs();
    let unhandled = ktest_unhandled_stub as *const () as u64;
    let pf = ktest_pf_stub as *const () as u64;
    unsafe {
        let idt = &mut *IDT.ptr();
        for slot in idt.0.iter_mut() {
            *slot = IdtEntry::gate(unhandled, cs);
        }
        idt.0[14] = IdtEntry::gate(pf, cs);
        let idtr = Idtr {
            limit: (core::mem::size_of::<IdtTable>() - 1) as u16,
            base: IDT.ptr() as u64,
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
    // LAPIC (0xFEE0_0000) sits above this QEMU map_end (~4 GiB of RAM),
    // so patch a physmap leaf we know exists: 2 MiB, inside the identity
    // rest / physmap and well under the 8 GiB cap.
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
