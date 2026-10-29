//! Scoped transient exception / alloc-error catcher for in-guest tests.
//! Compiles only with `kernel_tests` (AGENTS.md rule 9).
//!
//! Install, run a faulting op, either longjmp out or step RIP past the
//! instruction, restore. Nested catch is not supported. DESIGN §5.2 / §8.2.
//!
//! A catch window belongs to the CPU that armed it (`ARMED`): the hooks
//! act only there, and `intercept` only on a CPL-0 frame, so a fault on
//! another CPU, or one raised by ring 3, takes its normal path (invariant
//! I29). The window must not span a CPU migration, which nothing does
//! while no preempted thread changes CPU (invariant I36).
//!
//! It also holds the arch stall points production code calls under
//! `kernel_tests` to widen a race window ([`force_kernel_window`]).

use core::alloc::Layout;
use core::arch::global_asm;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicUsize, Ordering};

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

const NO_CATCH: Caught = Caught {
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
};

/// One `Caught` slot per CPU id.
const CPUS: usize = 64;

static KIND: AtomicU8 = AtomicU8::new(ST_OFF);
/// The arming CPU's token, `cpu_id + 1`, or 1 before per-CPU state is live
/// (`cell.rs`'s `owner_token` encoding); 0 when no catch is armed. Stored
/// before `KIND`'s Release store and cleared after `KIND`'s reset, so a
/// hook that loads `KIND` with Acquire sees this window's token.
static ARMED: AtomicU32 = AtomicU32::new(0);
static WANT: AtomicU8 = AtomicU8::new(0);
static SKIP_LEN: AtomicU8 = AtomicU8::new(0);
/// Written by `intercept` and read by `catch*`, both only on the armed CPU,
/// so no slot is taken by two CPUs.
static LAST: [IrqCell<Caught>; CPUS] = [const { IrqCell::new(NO_CATCH) }; CPUS];
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
    // SAFETY: this fn's `# Safety` (here): `p` is the `Option<F>` that
    // `with_thunk` stored, live on its frame and referred to by nothing else.
    let slot = unsafe { &mut *(p as *mut Option<F>) };
    slot.take().unwrap()();
}

fn with_thunk<F: FnOnce()>(f: F) -> i32 {
    let mut slot = Some(f);
    // Release: pairs with the Acquire swap in `vibeos_catch_thunk`.
    THUNK_DATA.store((&raw mut slot).cast(), Ordering::Release);
    // Release: pairs with the Acquire swap in `vibeos_catch_thunk`.
    THUNK_CALL.store(invoke::<F> as *const () as usize, Ordering::Release);
    // SAFETY: invariant: `THUNK_DATA` points at `slot`, live on this frame
    // for the call, and `THUNK_CALL` is `invoke::<F>`, the one reader that
    // matches it, so `vibeos_catch_thunk` runs `f` once; `vibeos_catch`
    // saves the context a longjmp returns to while this frame is live;
    // established here.
    let rc = unsafe { vibeos_catch() };
    // Release: pairs with the Acquire swap in `vibeos_catch_thunk`.
    THUNK_CALL.store(0, Ordering::Release);
    rc
}

#[unsafe(no_mangle)]
extern "C" fn vibeos_catch_thunk() {
    // Acquire: pairs with the Release stores in `with_thunk`.
    let call = THUNK_CALL.swap(0, Ordering::Acquire);
    if call != 0 {
        // Acquire: pairs with the Release store in `with_thunk`.
        let data = THUNK_DATA.swap(core::ptr::null_mut(), Ordering::Acquire);
        // SAFETY: invariant: a nonzero `THUNK_CALL` is an `invoke::<F>`
        // address and `THUNK_DATA` its `Option<F>`; established by
        // `arch::x86_64::catch::with_thunk`, the only nonzero store of each.
        let f: unsafe fn(*mut u8) = unsafe { core::mem::transmute(call) };
        // SAFETY: `invoke`'s `# Safety`, established by
        // `arch::x86_64::catch::with_thunk`: `data` is the matching
        // `Option<F>`.
        unsafe { f(data) };
    }
}

/// This CPU's token: `cpu_id + 1`, or 1 before per-CPU state is live.
fn token() -> u32 {
    x86::cpu_index().map(|i| i + 1).unwrap_or(1)
}

/// Arm a catch window of `kind` on this CPU.
fn arm(kind: u8) {
    // Relaxed: the Release store of `KIND` below publishes it; pairs with nothing.
    ARMED.store(token(), Ordering::Relaxed);
    // Release: pairs with the Acquire load of `KIND` in a hook (`armed_here`),
    // publishing `ARMED`, `WANT` and `SKIP_LEN` with it.
    KIND.store(kind, Ordering::Release);
}

/// Close the window: `KIND` first, then `ARMED`.
fn disarm() {
    // Release: pairs with the Acquire loads in `armed_here` and `catch_skip`.
    KIND.store(ST_OFF, Ordering::Release);
    // Relaxed: `KIND` closed the window first; pairs with nothing.
    ARMED.store(0, Ordering::Relaxed);
}

/// The one predicate every hook applies: the armed kind when this CPU armed
/// the window, and `ST_OFF` on any other CPU (invariant I29).
fn armed_here() -> u8 {
    // Acquire: pairs with the Release store in `arm`.
    let kind = KIND.load(Ordering::Acquire);
    // Relaxed: the Acquire load of `KIND` above orders it; pairs with nothing.
    if kind == ST_OFF || ARMED.load(Ordering::Relaxed) != token() {
        return ST_OFF;
    }
    kind
}

/// This CPU's `LAST` slot.
fn last() -> &'static IrqCell<Caught> {
    // `token() - 1` is a `cpu_id`, below 64 (`per_cpu_init`'s masks).
    &LAST[(token() - 1) as usize % CPUS]
}

fn record(frame: &TrapFrame) {
    let caught = Caught {
        vector: frame.vector as u8,
        error: frame.error_code,
        cr2: frame.cr2,
        frame: frame.iret,
        handler_rsp: x86::read_rsp(),
    };
    last().with(|last| *last = caught);
}

/// Install [`intercept`] as `arch::idt`'s exception intercept. `_start`
/// calls it right after `idt::init`.
pub fn init() {
    crate::arch::idt::set_intercept_hook(intercept);
}

/// Called by `arch::idt`'s dispatcher for vectors 0 to 31. `true` means
/// skip the body and iret (RIP already adjusted). Longjmp never returns.
/// Acts only on a CPL-0 frame on the CPU that armed the catch; on any other
/// it returns `false`, as when unarmed.
pub fn intercept(frame: &mut TrapFrame) -> bool {
    if frame.iret.cs & 3 != 0 {
        return false;
    }
    let vector = frame.vector as u8;
    let kind = armed_here();
    // Relaxed: `armed_here`'s Acquire load of `KIND` orders it; pairs with nothing.
    let want = WANT.load(Ordering::Relaxed);
    match kind {
        ST_OFF | ST_ALLOC | ST_PANIC => false,
        ST_VECTOR if want == vector => {
            record(frame);
            disarm();
            // SAFETY: invariant: an armed window lies inside `with_thunk`'s call,
            // so `vibeos_jmpbuf` holds the context `vibeos_catch` saved on a frame
            // that is still live; established by `arch::x86_64::catch::with_thunk`.
            unsafe { vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1) };
        }
        ST_SKIP if want == vector => {
            record(frame);
            // Relaxed: as `WANT` above; pairs with nothing.
            frame.iret.rip = frame
                .iret
                .rip
                .wrapping_add(SKIP_LEN.load(Ordering::Relaxed) as u64);
            disarm();
            true
        }
        ST_VECTOR | ST_SKIP => false,
        _ => false,
    }
}

pub fn catch<F: FnOnce()>(vector: u8, f: F) -> Option<Caught> {
    // Relaxed: the Release store of `KIND` in `arm` publishes it; pairs with nothing.
    WANT.store(vector, Ordering::Relaxed);
    arm(ST_VECTOR);
    let rc = with_thunk(f);
    disarm();
    if rc != 0 {
        Some(last().with(|c| *c))
    } else {
        None
    }
}

/// Run `f`; on `vector`, add `insn_len` to RIP and continue.
pub fn catch_skip<F: FnOnce()>(vector: u8, insn_len: u8, f: F) -> Option<Caught> {
    // Relaxed: the Release store of `KIND` in `arm` publishes it; pairs with nothing.
    WANT.store(vector, Ordering::Relaxed);
    // Relaxed: the Release store of `KIND` in `arm` publishes it; pairs with nothing.
    SKIP_LEN.store(insn_len, Ordering::Relaxed);
    arm(ST_SKIP);
    let rc = with_thunk(f);
    // Acquire: pairs with the Release store in `disarm` when `intercept` took the skip.
    let skipped = KIND.load(Ordering::Acquire);
    disarm();
    match skipped {
        ST_OFF if rc == 0 => Some(last().with(|c| *c)),
        _ => None,
    }
}

pub fn catch_alloc<F: FnOnce()>(f: F) -> bool {
    arm(ST_ALLOC);
    let rc = with_thunk(f);
    disarm();
    rc != 0
}

/// Catch a Rust `panic!` via longjmp. Used by `irqcell_reentry_panics`.
pub fn catch_panic<F: FnOnce()>(f: F) -> bool {
    arm(ST_PANIC);
    let rc = with_thunk(f);
    disarm();
    rc != 0
}

/// Longjmp out of an armed `catch_alloc`, on the CPU that armed it only.
pub fn on_alloc_error(_layout: Layout) {
    if armed_here() == ST_ALLOC {
        disarm();
        // SAFETY: invariant: an armed window lies inside `with_thunk`'s call,
        // so `vibeos_jmpbuf` holds the context `vibeos_catch` saved on a frame
        // that is still live; established by `arch::x86_64::catch::with_thunk`.
        unsafe { vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1) };
    }
}

/// Longjmp out of an armed `catch_panic`, on the CPU that armed it only.
pub fn on_panic() {
    if armed_here() == ST_PANIC {
        disarm();
        // SAFETY: invariant: an armed window lies inside `with_thunk`'s call,
        // so `vibeos_jmpbuf` holds the context `vibeos_catch` saved on a frame
        // that is still live; established by `arch::x86_64::catch::with_thunk`.
        unsafe { vibeos_longjmp(core::ptr::addr_of_mut!(vibeos_jmpbuf), 1) };
    }
}

/// `gs::force_kernel` calls [`force_kernel_window`] still holds.
static FK_STALLS: AtomicU32 = AtomicU32::new(0);

/// Hold the next `n` `gs::force_kernel` calls in [`force_kernel_window`];
/// 0 disarms.
pub fn arm_force_kernel_window(n: u32) {
    // Release: pairs with the update in `force_kernel_window` and the load below.
    FK_STALLS.store(n, Ordering::Release);
}

/// Stalls not yet taken.
pub fn force_kernel_windows_left() -> u32 {
    // Acquire: pairs with the AcqRel update in `force_kernel_window`.
    FK_STALLS.load(Ordering::Acquire)
}

/// In `gs::force_kernel`, once the data segments are loaded and before
/// `GS_BASE` is written back: while armed, spin 2 ms so an interrupt lands
/// inside that window. It reads no `gs:` operand, since `mov gs` zeroed
/// `GS_BASE`.
pub fn force_kernel_window() {
    // AcqRel, Acquire on failure: pairs with the Release store in `arm_force_kernel_window`.
    if FK_STALLS
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_err()
    {
        return;
    }
    let t0 = crate::time_init::now_ns();
    while crate::time_init::now_ns().saturating_sub(t0) < 2_000_000 {
        core::hint::spin_loop();
    }
}
