//! Scoped transient exception / alloc-error catcher for in-guest tests.
//!
//! Install, run a faulting op, either longjmp out or step RIP past the
//! instruction, restore. Nested catch is not supported. DESIGN §5.2 / §8.2.

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::alloc::Layout;
use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::ptr;

use vibeos::desc::InterruptFrame;

use crate::x86;

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

#[derive(Clone, Copy)]
enum State {
    Off,
    Vector(u8),
    Skip { vector: u8, len: u8 },
    Alloc,
}

#[derive(Clone, Copy, Debug)]
pub struct Caught {
    pub vector: u8,
    pub error: u64,
    pub cr2: u64,
    pub frame: InterruptFrame,
    pub handler_rsp: u64,
}

static STATE: Cell<State> = Cell::new(State::Off);
static LAST: Cell<Caught> = Cell::new(Caught {
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
static THUNK_DATA: Cell<*mut u8> = Cell::new(core::ptr::null_mut());
static THUNK_CALL: Cell<Option<unsafe fn(*mut u8)>> = Cell::new(None);

unsafe extern "C" {
    fn vibeos_catch() -> i32;
    fn vibeos_longjmp(buf: *mut JmpBuf, val: i32) -> !;
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

unsafe fn invoke<F: FnOnce()>(p: *mut u8) {
    let slot = unsafe { &mut *(p as *mut Option<F>) };
    slot.take().unwrap()();
}

fn with_thunk<F: FnOnce()>(f: F) -> i32 {
    let mut slot = Some(f);
    unsafe {
        ptr::write(THUNK_DATA.ptr(), (&raw mut slot).cast());
        ptr::write(THUNK_CALL.ptr(), Some(invoke::<F>));
        let rc = vibeos_catch();
        ptr::write(THUNK_CALL.ptr(), None);
        rc
    }
}

#[unsafe(no_mangle)]
extern "C" fn vibeos_catch_thunk() {
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

fn record(vector: u8, frame: &InterruptFrame, err: u64) {
    let caught = Caught {
        vector,
        error: err,
        cr2: x86::read_cr2(),
        frame: *frame,
        handler_rsp: x86::read_rsp(),
    };
    unsafe { ptr::write(LAST.ptr(), caught) };
}

/// Called from every IDT handler. `true` means the handler should iret
/// (RIP already adjusted). Longjmp never returns.
pub fn intercept(vector: u8, frame: &mut InterruptFrame, err: u64) -> bool {
    let state = unsafe { ptr::read(STATE.ptr()) };
    match state {
        State::Off | State::Alloc => false,
        State::Vector(want) if want == vector => {
            record(vector, frame, err);
            unsafe { ptr::write(STATE.ptr(), State::Off) };
            unsafe { vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1) };
        }
        State::Skip { vector: want, len } if want == vector => {
            record(vector, frame, err);
            frame.rip = frame.rip.wrapping_add(len as u64);
            unsafe { ptr::write(STATE.ptr(), State::Off) };
            true
        }
        State::Vector(_) | State::Skip { .. } => false,
    }
}

pub fn catch<F: FnOnce()>(vector: u8, f: F) -> Option<Caught> {
    unsafe { ptr::write(STATE.ptr(), State::Vector(vector)) };
    let rc = with_thunk(f);
    unsafe { ptr::write(STATE.ptr(), State::Off) };
    if rc != 0 {
        Some(unsafe { ptr::read(LAST.ptr()) })
    } else {
        None
    }
}

/// Run `f`; on `vector`, add `insn_len` to RIP and continue.
pub fn catch_skip<F: FnOnce()>(vector: u8, insn_len: u8, f: F) -> Option<Caught> {
    unsafe {
        ptr::write(
            STATE.ptr(),
            State::Skip {
                vector,
                len: insn_len,
            },
        )
    };
    let rc = with_thunk(f);
    let skipped = unsafe { ptr::read(STATE.ptr()) };
    unsafe { ptr::write(STATE.ptr(), State::Off) };
    match skipped {
        State::Off if rc == 0 => Some(unsafe { ptr::read(LAST.ptr()) }),
        _ => None,
    }
}

pub fn catch_alloc<F: FnOnce()>(f: F) -> bool {
    unsafe { ptr::write(STATE.ptr(), State::Alloc) };
    let rc = with_thunk(f);
    unsafe { ptr::write(STATE.ptr(), State::Off) };
    rc != 0
}

pub fn on_alloc_error(layout: Layout) {
    unsafe {
        if let State::Alloc = ptr::read(STATE.ptr()) {
            ptr::write(STATE.ptr(), State::Off);
            vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1);
        }
    }
    let _ = layout;
}
