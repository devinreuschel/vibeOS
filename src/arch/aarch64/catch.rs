//! Scoped fault catcher for in-guest tests (AGENTS.md rule 9).

use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use super::vectors::TrapFrame;

const ST_OFF: u8 = 0;
const ST_VECTOR: u8 = 1;
const ST_DABT: u8 = 2;
const ST_PANIC: u8 = 3;
const ST_ALLOC: u8 = 4;

#[repr(C)]
struct JmpBuf {
    x19: u64,
    x20: u64,
    x21: u64,
    x22: u64,
    x23: u64,
    x24: u64,
    x25: u64,
    x26: u64,
    x27: u64,
    x28: u64,
    x29: u64,
    sp: u64,
    lr: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Caught {
    pub far: u64,
    pub esr: u64,
    #[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
    pub elr: u64,
    pub handler_rsp: u64,
}

static KIND: AtomicU8 = AtomicU8::new(ST_OFF);
static ARMED: AtomicU32 = AtomicU32::new(0);
static GOT_FAR: AtomicU64 = AtomicU64::new(0);
static GOT_ESR: AtomicU64 = AtomicU64::new(0);
static GOT_ELR: AtomicU64 = AtomicU64::new(0);
static GOT_SP: AtomicU64 = AtomicU64::new(0);

struct CatchBuf(UnsafeCell<JmpBuf>);

// SAFETY: `CatchBuf` gives one holder `&mut JmpBuf` (`JmpBuf: Send`);
// the catch window is armed with IRQs off on one CPU; established here.
unsafe impl Sync for CatchBuf {}

static BUF: CatchBuf = CatchBuf(UnsafeCell::new(JmpBuf {
    x19: 0,
    x20: 0,
    x21: 0,
    x22: 0,
    x23: 0,
    x24: 0,
    x25: 0,
    x26: 0,
    x27: 0,
    x28: 0,
    x29: 0,
    sp: 0,
    lr: 0,
}));

global_asm!(
    ".global vibeos_setjmp",
    ".global vibeos_longjmp",
    "vibeos_setjmp:",
    "    stp x19, x20, [x0]",
    "    stp x21, x22, [x0, #16]",
    "    stp x23, x24, [x0, #32]",
    "    stp x25, x26, [x0, #48]",
    "    stp x27, x28, [x0, #64]",
    "    mov x2, sp",
    "    stp x29, x2, [x0, #80]",
    "    str x30, [x0, #96]",
    "    mov x0, #0",
    "    ret",
    "vibeos_longjmp:",
    "    ldp x19, x20, [x0]",
    "    ldp x21, x22, [x0, #16]",
    "    ldp x23, x24, [x0, #32]",
    "    ldp x25, x26, [x0, #48]",
    "    ldp x27, x28, [x0, #64]",
    "    ldp x29, x2, [x0, #80]",
    "    ldr x30, [x0, #96]",
    "    mov sp, x2",
    "    mov x0, x1",
    "    ret",
);

unsafe extern "C" {
    fn vibeos_setjmp(buf: *mut JmpBuf) -> i32;
    fn vibeos_longjmp(buf: *mut JmpBuf, v: i32) -> !;
}

pub fn init() {}

pub fn catch_dabt<F: FnOnce()>(f: F) -> Option<Caught> {
    catch_kind(ST_DABT, f)
}

pub fn catch<F: FnOnce()>(f: F) -> Option<Caught> {
    catch_kind(ST_VECTOR, f)
}

fn catch_kind<F: FnOnce()>(kind: u8, f: F) -> Option<Caught> {
    // Release: pairs with the Acquire load in `overflow` / `intercept`.
    KIND.store(kind, Ordering::Release);
    // Release: pairs with the Acquire load in `overflow`.
    ARMED.store(1, Ordering::Release);
    // SAFETY: `vibeos_jmpbuf` is this CPU's catch buffer; the test holds
    // IRQs off; established here.
    let rc = unsafe { vibeos_setjmp(BUF.0.get()) };
    if rc == 0 {
        f();
        // Release: pairs with the Acquire load in `overflow` / `intercept`.
        KIND.store(ST_OFF, Ordering::Release);
        // Release: pairs with nothing.
        ARMED.store(0, Ordering::Release);
        return None;
    }
    // Release: pairs with the Acquire load in `overflow` / `intercept`.
    KIND.store(ST_OFF, Ordering::Release);
    // Release: pairs with nothing.
    ARMED.store(0, Ordering::Release);
    Some(Caught {
        // Relaxed: pairs with the Relaxed stores in `overflow` / `intercept` on this CPU.
        far: GOT_FAR.load(Ordering::Relaxed),
        // Relaxed: pairs with the Relaxed stores in `overflow` / `intercept` on this CPU.
        esr: GOT_ESR.load(Ordering::Relaxed),
        // Relaxed: pairs with the Relaxed stores in `overflow` / `intercept` on this CPU.
        elr: GOT_ELR.load(Ordering::Relaxed),
        // Relaxed: pairs with the Relaxed stores in `overflow` / `intercept` on this CPU.
        handler_rsp: GOT_SP.load(Ordering::Relaxed),
    })
}

pub fn intercept(frame: &mut TrapFrame) -> bool {
    // Acquire: pairs with the Release store in `catch_dabt`.
    if KIND.load(Ordering::Acquire) != ST_DABT {
        return false;
    }
    // Relaxed: pairs with the Relaxed loads in `catch_dabt` on this CPU.
    GOT_FAR.store(frame.far, Ordering::Relaxed);
    // Relaxed: pairs with the Relaxed loads in `catch_dabt` on this CPU.
    GOT_ESR.store(frame.esr, Ordering::Relaxed);
    // Relaxed: pairs with the Relaxed loads in `catch_dabt` on this CPU.
    GOT_ELR.store(frame.elr, Ordering::Relaxed);
    // Relaxed: pairs with the Relaxed loads in `catch_dabt` on this CPU.
    GOT_SP.store(frame.sp, Ordering::Relaxed);
    // SAFETY: `vibeos_jmpbuf` was filled by `setjmp` on this CPU; established here.
    unsafe { vibeos_longjmp(BUF.0.get(), 1) };
}

pub fn overflow(far: u64, esr: u64, elr: u64, sp: u64) -> bool {
    // Acquire: pairs with the Release store in `catch`.
    if KIND.load(Ordering::Acquire) != ST_VECTOR {
        return false;
    }
    // Relaxed: pairs with the Relaxed loads in `catch` on this CPU.
    GOT_FAR.store(far, Ordering::Relaxed);
    // Relaxed: pairs with the Relaxed loads in `catch` on this CPU.
    GOT_ESR.store(esr, Ordering::Relaxed);
    // Relaxed: pairs with the Relaxed loads in `catch` on this CPU.
    GOT_ELR.store(elr, Ordering::Relaxed);
    // Relaxed: pairs with the Relaxed loads in `catch` on this CPU.
    GOT_SP.store(sp, Ordering::Relaxed);
    // SAFETY: `vibeos_jmpbuf` was filled by `setjmp` on this CPU; established here.
    unsafe { vibeos_longjmp(BUF.0.get(), 1) };
}

/// Catch a Rust `panic!` via longjmp. Used by `current_at_if1`.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "kernel_tests catch window")
)]
pub fn catch_panic<F: FnOnce()>(f: F) -> bool {
    catch_kind(ST_PANIC, f).is_some()
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "kernel_tests alloc-failure window")
)]
pub fn catch_alloc<F: FnOnce()>(f: F) -> bool {
    catch_kind(ST_ALLOC, f).is_some()
}

/// Longjmp out of an armed `catch_panic`, on the CPU that armed it only.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "kernel_tests catch window")
)]
pub fn on_panic() {
    // Acquire: pairs with the Release store in `catch_kind`.
    if KIND.load(Ordering::Acquire) != ST_PANIC {
        return;
    }
    // Release: pairs with the Acquire load in `overflow` / `intercept`.
    KIND.store(ST_OFF, Ordering::Release);
    // Release: pairs with nothing.
    ARMED.store(0, Ordering::Release);
    // SAFETY: an armed `catch_panic` window lies inside `catch_kind`'s
    // `vibeos_setjmp` call, so `BUF` holds the context that call saved on
    // a frame that is still live; established by
    // `arch::aarch64::catch::catch_kind`.
    unsafe { vibeos_longjmp(BUF.0.get(), 1) };
}

/// Longjmp out of an armed `catch_alloc`, on the CPU that armed it only.
pub fn on_alloc_error(_layout: core::alloc::Layout) {
    // Acquire: pairs with the Release store in `catch_kind`.
    if KIND.load(Ordering::Acquire) != ST_ALLOC {
        return;
    }
    // Release: pairs with the Acquire load in `catch_kind` after the longjmp.
    KIND.store(ST_OFF, Ordering::Release);
    // Release: pairs with nothing.
    ARMED.store(0, Ordering::Release);
    // SAFETY: an armed `catch_alloc` window lies inside `catch_kind`'s
    // `vibeos_setjmp` call, so `BUF` holds the context that call saved on
    // a frame that is still live; established by `arch::aarch64::catch::catch_kind`.
    unsafe { vibeos_longjmp(BUF.0.get(), 1) };
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn force_kernel_window() {}
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn force_kernel_windows_left() -> u32 {
    0
}
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn arm_force_kernel_window(_n: u32) {}
