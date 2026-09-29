//! Scoped transient exception / alloc-error catcher for in-guest tests.
//!
//! Install, run a faulting op, either longjmp out or step RIP past the
//! instruction, restore. Nested catch is not supported. DESIGN §5.2 / §8.2.

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::alloc::Layout;
use core::arch::global_asm;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};

use vibeos::desc::InterruptFrame;

use crate::arch::idt::TrapFrame;

use crate::cell::IrqCell;
use crate::x86;

const ST_OFF: u8 = 0;
const ST_VECTOR: u8 = 1;
const ST_SKIP: u8 = 2;
const ST_ALLOC: u8 = 3;
const ST_PANIC: u8 = 4;

#[repr(C)]
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

#[derive(Clone, Copy, Debug)]
pub struct Caught {
    pub vector: u8,
    pub error: u64,
    pub cr2: u64,
    pub frame: InterruptFrame,
    pub handler_rsp: u64,
}

static KIND: AtomicU8 = AtomicU8::new(ST_OFF);
static WANT: AtomicU8 = AtomicU8::new(0);
static SKIP_LEN: AtomicU8 = AtomicU8::new(0);
static LAST: IrqCell<Caught> = IrqCell::new(Caught {
    vector: 0,
    error: 0,
    cr2: 0,
    frame: InterruptFrame {
        rip: 0,
        cs: 0,
        rflags: 0,
        rsp: 0,
        ss: 0,
    },
    handler_rsp: 0,
});
static THUNK_DATA: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static THUNK_CALL: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" {
    fn vibeos_catch() -> i32;
    fn vibeos_longjmp(buf: *mut JmpBuf, val: i32) -> !;
    /// Asm-owned setjmp buffer. The only remaining `static mut` (Q3).
    static mut vibeos_jmpbuf: JmpBuf;
}

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
        call vibeos_catch_thunk
        xor eax, eax
        pop rbp
        ret
    1:
        mov eax, 1
        pop rbp
        ret

    .popsection
    "#
);

/// # Safety
/// `p` is `Option<F>` for the active catch thunk.
unsafe fn invoke<F: FnOnce()>(p: *mut u8) {
    let slot = unsafe { &mut *(p as *mut Option<F>) };
    slot.take().unwrap()();
}

fn with_thunk<F: FnOnce()>(f: F) -> i32 {
    let mut slot = Some(f);
    THUNK_DATA.store((&raw mut slot).cast(), Ordering::Release);
    THUNK_CALL.store(invoke::<F> as *const () as usize, Ordering::Release);
    let rc = unsafe { vibeos_catch() };
    THUNK_CALL.store(0, Ordering::Release);
    rc
}

#[unsafe(no_mangle)]
extern "C" fn vibeos_catch_thunk() {
    let call = THUNK_CALL.swap(0, Ordering::Acquire);
    if call != 0 {
        let data = THUNK_DATA.swap(core::ptr::null_mut(), Ordering::Acquire);
        let f: unsafe fn(*mut u8) = unsafe { core::mem::transmute(call) };
        unsafe { f(data) };
    }
}

fn record(frame: &TrapFrame) {
    let caught = Caught {
        vector: frame.vector as u8,
        error: frame.error_code,
        cr2: frame.cr2,
        frame: frame.iret,
        handler_rsp: x86::read_rsp(),
    };
    LAST.with(|last| *last = caught);
}

/// Called by `arch::idt`'s dispatcher for vectors 0 to 31. `true` means
/// skip the body and iret (RIP already adjusted). Longjmp never returns.
pub fn intercept(frame: &mut TrapFrame) -> bool {
    let vector = frame.vector as u8;
    let kind = KIND.load(Ordering::Acquire);
    let want = WANT.load(Ordering::Relaxed);
    match kind {
        ST_OFF | ST_ALLOC | ST_PANIC => false,
        ST_VECTOR if want == vector => {
            record(frame);
            KIND.store(ST_OFF, Ordering::Release);
            unsafe { vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1) };
        }
        ST_SKIP if want == vector => {
            record(frame);
            frame.iret.rip = frame
                .iret
                .rip
                .wrapping_add(SKIP_LEN.load(Ordering::Relaxed) as u64);
            KIND.store(ST_OFF, Ordering::Release);
            true
        }
        ST_VECTOR | ST_SKIP => false,
        _ => false,
    }
}

pub fn catch<F: FnOnce()>(vector: u8, f: F) -> Option<Caught> {
    WANT.store(vector, Ordering::Relaxed);
    KIND.store(ST_VECTOR, Ordering::Release);
    let rc = with_thunk(f);
    KIND.store(ST_OFF, Ordering::Release);
    if rc != 0 {
        Some(LAST.with(|c| *c))
    } else {
        None
    }
}

/// Run `f`; on `vector`, add `insn_len` to RIP and continue.
pub fn catch_skip<F: FnOnce()>(vector: u8, insn_len: u8, f: F) -> Option<Caught> {
    WANT.store(vector, Ordering::Relaxed);
    SKIP_LEN.store(insn_len, Ordering::Relaxed);
    KIND.store(ST_SKIP, Ordering::Release);
    let rc = with_thunk(f);
    let skipped = KIND.load(Ordering::Acquire);
    KIND.store(ST_OFF, Ordering::Release);
    match skipped {
        ST_OFF if rc == 0 => Some(LAST.with(|c| *c)),
        _ => None,
    }
}

pub fn catch_alloc<F: FnOnce()>(f: F) -> bool {
    KIND.store(ST_ALLOC, Ordering::Release);
    let rc = with_thunk(f);
    KIND.store(ST_OFF, Ordering::Release);
    rc != 0
}

/// Catch a Rust `panic!` via longjmp. Used by `irqcell_reentry_panics`.
pub fn catch_panic<F: FnOnce()>(f: F) -> bool {
    KIND.store(ST_PANIC, Ordering::Release);
    let rc = with_thunk(f);
    KIND.store(ST_OFF, Ordering::Release);
    rc != 0
}

pub fn on_alloc_error(layout: Layout) {
    if KIND.load(Ordering::Acquire) == ST_ALLOC {
        KIND.store(ST_OFF, Ordering::Release);
        unsafe { vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1) };
    }
    let _ = layout;
}

pub fn on_panic() {
    if KIND.load(Ordering::Acquire) == ST_PANIC {
        KIND.store(ST_OFF, Ordering::Release);
        unsafe { vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1) };
    }
}
