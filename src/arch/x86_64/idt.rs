//! IDT, generated entry stubs, and exception bodies. DESIGN §5.2, §5.10.
//!
//! Every vector 0 to 255 enters through a stub that this module generates
//! from [`ROWS`]. A stub clears RFLAGS.AC, pushes a zero where the CPU
//! pushes no error code and its vector, then jumps to one of two entry
//! paths, which build a
//! [`TrapFrame`], make the `swapgs` decision, save CR2 (`#PF`) or DR6
//! (`#DB`) into the frame, and call [`trap_dispatch`]. The dispatcher runs
//! the body that [`set_handler`] registered, or [`default_body`]. Unhandled
//! vectors dump and halt. `#BP` logs and returns. `#UD`/`#GP`/`#PF`/`#DF`/
//! `#MC` dump and halt.

use core::arch::global_asm;
use core::mem::{offset_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use vibeos::desc::{IdtEntry, InterruptFrame, IstSlot, KERNEL_CS};
use vibeos::per_cpu::PerCpu;
use vibeos::syscall::UserFrame;
use vibeos::vectors;

use crate::arch::gs;
use crate::arch::pic;
use crate::cell::IrqCell;
use crate::serial;
use crate::x86::{self, DtPtr};

#[repr(C, align(16))]
struct Idt([IdtEntry; 256]);

static IDT: IrqCell<Idt> = IrqCell::new(Idt([IdtEntry::EMPTY; 256]));

/// What a generated stub saves, low address first. The five low words
/// are the stub's; the rest is Linux's `user_regs_struct` ([`UserFrame`]),
/// whose `orig_rax` slot holds the CPU's error code (or the stub's zero)
/// on entry. The entry copies it to `error_code` and stores `-1` there,
/// as Linux does for an entry that is not a syscall.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TrapFrame {
    pub pad: u64,
    pub vector: u64,
    pub error_code: u64,
    /// CR2 as the entry found it for `#PF`, else 0.
    pub cr2: u64,
    /// DR6 as the entry found it for `#DB`, else 0.
    pub dr6: u64,
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub orig_rax: u64,
    pub iret: InterruptFrame,
}

const F_VECTOR: usize = offset_of!(TrapFrame, vector);
const F_ERROR: usize = offset_of!(TrapFrame, error_code);
const F_CR2: usize = offset_of!(TrapFrame, cr2);
const F_DR6: usize = offset_of!(TrapFrame, dr6);
const F_USER: usize = offset_of!(TrapFrame, r15);
const F_ORIG_RAX: usize = offset_of!(TrapFrame, orig_rax);
const F_CS: usize = offset_of!(TrapFrame, iret) + offset_of!(InterruptFrame, cs);
/// The words below the pushed GPRs: `pad` to `dr6`.
const LOW_WORDS: usize = F_USER;

const _: () = {
    assert!(offset_of!(TrapFrame, pad) == 0);
    assert!(F_VECTOR == 8);
    assert!(F_ERROR == 16);
    assert!(F_CR2 == 24);
    assert!(F_DR6 == 32);
    assert!(F_USER == 40);
    assert!(offset_of!(TrapFrame, rdi) == 152);
    assert!(F_ORIG_RAX == 160);
    assert!(offset_of!(TrapFrame, iret) == 168);
    assert!(size_of::<TrapFrame>() == 208);
    // 26 words below the CPU's 16-byte-aligned base: RSP stays aligned at
    // the dispatcher `call`.
    assert!(size_of::<TrapFrame>().is_multiple_of(16));
    // `user()` views bytes F_USER.. as a UserFrame.
    assert!(size_of::<TrapFrame>() - F_USER == size_of::<UserFrame>());
    assert!(offset_of!(UserFrame, rbx) + F_USER == offset_of!(TrapFrame, rbx));
    assert!(offset_of!(UserFrame, rdi) + F_USER == offset_of!(TrapFrame, rdi));
    assert!(offset_of!(UserFrame, orig_rax) + F_USER == F_ORIG_RAX);
    assert!(offset_of!(UserFrame, rip) + F_USER == offset_of!(TrapFrame, iret));
    assert!(offset_of!(UserFrame, cs) + F_USER == F_CS);
    assert!(
        offset_of!(UserFrame, ss) + F_USER
            == offset_of!(TrapFrame, iret) + offset_of!(InterruptFrame, ss)
    );
};

impl TrapFrame {
    /// A frame for `vector` over the user context `user`, for a ring-3 fault
    /// that no stub saved: the syscall exit's non-canonical RIP, and a fault
    /// on a return-to-user `iretq`.
    pub fn for_user(vector: u8, error_code: u64, user: &UserFrame) -> Self {
        let mut f = Self {
            pad: 0,
            vector: u64::from(vector),
            error_code,
            cr2: 0,
            dr6: 0,
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbp: 0,
            rbx: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rax: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            orig_rax: 0,
            iret: InterruptFrame {
                rip: 0,
                cs: 0,
                rflags: 0,
                rsp: 0,
                ss: 0,
            },
        };
        *f.user_mut() = *user;
        f
    }

    #[inline]
    pub fn user_mode(&self) -> bool {
        gs::from_user(self.user().cs)
    }

    /// The 21 `user_regs_struct` words, `r15` to `ss`: for a CS.RPL 3
    /// frame, the thread's user frame at the top of its kernel stack.
    pub fn user(&self) -> &UserFrame {
        // SAFETY: invariant: bytes `F_USER..` of a `TrapFrame` have
        // `UserFrame`'s layout, and a pointer derived from `&self` covers
        // the whole frame; established by the const block after
        // `arch::idt::TrapFrame`.
        unsafe {
            &*ptr::from_ref(self)
                .cast::<u8>()
                .add(F_USER)
                .cast::<UserFrame>()
        }
    }

    /// Mutable form of [`TrapFrame::user`]. The exit restores what it holds.
    pub fn user_mut(&mut self) -> &mut UserFrame {
        // SAFETY: invariant: bytes `F_USER..` of a `TrapFrame` have
        // `UserFrame`'s layout, and a pointer derived from `&mut self` is
        // unique for the borrow; established by the const block after
        // `arch::idt::TrapFrame`.
        unsafe {
            &mut *ptr::from_mut(self)
                .cast::<u8>()
                .add(F_USER)
                .cast::<UserFrame>()
        }
    }
}

/// A vector's body. Runs on the kernel GS base with the frame the stub
/// built; the exit restores the frame's registers and `iretq`s.
pub type TrapBody = fn(&mut TrapFrame);

/// One row per vector: the stub and gate that [`init`] builds for it.
#[derive(Clone, Copy)]
struct Row {
    /// The vector this row describes; the `const` block after [`ROWS`]
    /// checks that each of 0 to 255 has exactly one row.
    vector: u8,
    /// The CPU pushes an error code, so the stub pushes no zero.
    err: bool,
    /// Hardware IST field, 0 for none.
    ist: u8,
    dpl: u8,
    /// Enters through the paranoid (IST) path.
    paranoid: bool,
}

const fn build_rows() -> [Row; 256] {
    let mut rows = [Row {
        vector: 0,
        err: false,
        ist: 0,
        dpl: 0,
        paranoid: false,
    }; 256];
    let mut v = 0;
    while v < 256 {
        let ist = ist_for(v as u8);
        rows[v] = Row {
            vector: v as u8,
            err: vectors::pushes_error_code(v as u8),
            ist,
            // `int3` from ring 3 reaches its body; any other `int n` from
            // ring 3 raises `#GP`.
            dpl: if v as u8 == vectors::BP { 3 } else { 0 },
            paranoid: ist != 0,
        };
        v += 1;
    }
    rows
}

const ROWS: [Row; 256] = build_rows();

// The vector table has exactly one row for each vector 0 to 255: the build
// fails on a missing or doubled row (ROADMAP §10.3, INTERRUPTS.md §5.3).
const _: () = {
    assert!(ROWS.len() == 256);
    let mut want = 0;
    while want < 256 {
        let mut n = 0;
        let mut i = 0;
        while i < ROWS.len() {
            if ROWS[i].vector as usize == want {
                n += 1;
            }
            i += 1;
        }
        assert!(n == 1, "idt: the vector table needs one row per vector");
        want += 1;
    }
};

/// Bit `v` set when `ROWS[v].err`.
const ERR_MASK: u32 = {
    let mut m = 0u32;
    let mut v = 0;
    while v < 256 {
        if ROWS[v].err {
            assert!(
                v < 32,
                "the stub generator reads error codes from a u32 mask"
            );
            m |= 1 << v;
        }
        v += 1;
    }
    m
};

/// Bit `v` set when `ROWS[v].paranoid`.
const PARANOID_MASK: u32 = {
    let mut m = 0u32;
    let mut v = 0;
    while v < 256 {
        if ROWS[v].paranoid {
            assert!(v < 32, "the stub generator reads IST rows from a u32 mask");
            m |= 1 << v;
        }
        v += 1;
    }
    m
};

/// Bytes from one stub to the next.
const STUB_STRIDE: usize = 32;

/// `clac`, the first instruction of every stub; `#UD` without SMAP.
const CLAC: [u8; 3] = [0x0F, 0x01, 0xCA];

unsafe extern "C" {
    /// First of 256 stubs, `STUB_STRIDE` bytes apart.
    static vibeos_trap_stubs: [u8; 256 * STUB_STRIDE];
}

// The stubs and both entry paths. A stub is `clac`, `cld`, a zero where
// the CPU pushes no error code, its vector, and a jump; `.org` fails the
// build if one outgrows the stride. Bytes are spelled out so `init` can
// check them. A gate skips the 3-byte `clac` where the CPU has no SMAP.
// Neither entry path writes `rbp` before the `call`, so a backtrace from a
// body walks on into the interrupted frames.
global_asm!(
    r#"
    .macro vibeos_trap_stub v
        .byte 0x0F, 0x01, 0xCA
        .byte 0xFC
        .if (\v) >= 32
        .byte 0x6A, 0x00
        .elseif (({err_mask} >> ((\v) & 31)) & 1) == 0
        .byte 0x6A, 0x00
        .endif
        .if (\v) < 128
        .byte 0x6A, (\v)
        .else
        .byte 0x68
        .long (\v)
        .endif
        .if (\v) >= 32
        jmp vibeos_trap_entry
        .elseif (({paranoid_mask} >> ((\v) & 31)) & 1) == 0
        jmp vibeos_trap_entry
        .else
        jmp vibeos_trap_entry_ist
        .endif
        .org vibeos_trap_stubs + ((\v) + 1) * {stride}, 0xCC
    .endm
    .macro vibeos_trap_stub4 v
        vibeos_trap_stub (\v)
        vibeos_trap_stub ((\v) + 1)
        vibeos_trap_stub ((\v) + 2)
        vibeos_trap_stub ((\v) + 3)
    .endm
    .macro vibeos_trap_stub16 v
        vibeos_trap_stub4 (\v)
        vibeos_trap_stub4 ((\v) + 4)
        vibeos_trap_stub4 ((\v) + 8)
        vibeos_trap_stub4 ((\v) + 12)
    .endm
    .macro vibeos_trap_stub64 v
        vibeos_trap_stub16 (\v)
        vibeos_trap_stub16 ((\v) + 16)
        vibeos_trap_stub16 ((\v) + 32)
        vibeos_trap_stub16 ((\v) + 48)
    .endm

    .pushsection .text
    .balign 32
    .global vibeos_trap_stubs
    vibeos_trap_stubs:
        vibeos_trap_stub64 0
        vibeos_trap_stub64 64
        vibeos_trap_stub64 128
        vibeos_trap_stub64 192

    // Save the GPRs under the vector and error-code words, fill the low
    // words, and leave the vector in rdi.
    .macro vibeos_trap_save
        xchg rdi, [rsp]
        push rsi
        push rdx
        push rcx
        push rax
        push r8
        push r9
        push r10
        push r11
        push rbx
        push rbp
        push r12
        push r13
        push r14
        push r15
        sub rsp, {low}
        mov qword ptr [rsp], 0
        mov [rsp + {vector}], rdi
        mov rax, [rsp + {orig_rax}]
        mov [rsp + {error}], rax
        mov qword ptr [rsp + {orig_rax}], -1
    .endm

    // IF is off from here to the iretq (AGENTS.md rule 2).
    .macro vibeos_trap_exit_cli
        cli
        .if {debug}
        pushfq
        test byte ptr [rsp + 1], 2
        lea rsp, [rsp + 8]
        jz 91f
        ud2
    91:
        .endif
    .endm

    .macro vibeos_trap_restore
        add rsp, {low}
        pop r15
        pop r14
        pop r13
        pop r12
        pop rbp
        pop rbx
        pop r11
        pop r10
        pop r9
        pop r8
        pop rax
        pop rcx
        pop rdx
        pop rsi
        pop rdi
        add rsp, 8
    .endm

    .balign 16
    .global vibeos_trap_entry
    vibeos_trap_entry:
        vibeos_trap_save
        test byte ptr [rsp + {cs}], 3
        jz 1f
        swapgs
    1:
        xor eax, eax
        cmp edi, {pf}
        jne 2f
        mov rax, cr2
    2:
        mov [rsp + {cr2}], rax
        mov qword ptr [rsp + {dr6}], 0
    // An IST vector taken at CPL 3 joins here on the thread's kernel stack.
    .Lvibeos_trap_call:
        mov rdi, rsp
        call {dispatch}
        vibeos_trap_exit_cli
        test byte ptr [rsp + {cs}], 3
        jz 3f
        swapgs
    3:
        vibeos_trap_restore
    .global vibeos_trap_iret
    vibeos_trap_iret:
        iretq

    // NMI, #DB, #DF, #MC. A CPL-3 frame swaps by CS.RPL, saves DR6 (#DB)
    // and resets it to its idle value, copies the whole frame from the IST
    // stack to the top of the thread's kernel stack, where its tail is the
    // thread's user frame, and continues there as any CPL-3 entry does,
    // leaving the IST stack free (DESIGN §5.10 rule 3). A CPL-0 frame can
    // interrupt the syscall entry or exit with the user GS base loaded, so
    // it swaps only when GS_BASE is not a kernel (negative) address; #DF,
    // whose saved CS is undefined, always decides from the sign. ebx
    // (callee-saved) keeps that decision across the call, and the IST exit
    // mirrors it.
    .balign 16
    .global vibeos_trap_entry_ist
    vibeos_trap_entry_ist:
        vibeos_trap_save
        xor ebx, ebx
        cmp edi, {df}
        je 1f
        test byte ptr [rsp + {cs}], 3
        jz 1f
        swapgs
        mov qword ptr [rsp + {cr2}], 0
        mov qword ptr [rsp + {dr6}], 0
        cmp edi, {db}
        jne 6f
        mov rax, dr6
        mov [rsp + {dr6}], rax
        mov eax, {dr6_idle}
        mov dr6, rax
    6:
        mov rsi, rsp
        mov rdi, qword ptr gs:[{ksp}]
        sub rdi, {frame_bytes}
        mov rdx, rdi
        mov ecx, {frame_words}
        rep movsq
        mov rsp, rdx
        jmp .Lvibeos_trap_call
    1:
        mov ecx, {gs_base}
        rdmsr
        test edx, edx
        js 2f
        swapgs
        mov ebx, 1
    2:
        mov qword ptr [rsp + {cr2}], 0
        mov qword ptr [rsp + {dr6}], 0
        cmp edi, {db}
        jne 3f
        mov rax, dr6
        mov [rsp + {dr6}], rax
        mov eax, {dr6_idle}
        mov dr6, rax
    3:
        mov rdi, rsp
        call {dispatch}
        vibeos_trap_exit_cli
        test ebx, ebx
        jz 5f
        swapgs
    5:
        vibeos_trap_restore
    .global vibeos_trap_iret_ist
    vibeos_trap_iret_ist:
        iretq
    .popsection
    "#,
    err_mask = const ERR_MASK,
    paranoid_mask = const PARANOID_MASK,
    stride = const STUB_STRIDE,
    low = const LOW_WORDS,
    vector = const F_VECTOR,
    error = const F_ERROR,
    orig_rax = const F_ORIG_RAX,
    cr2 = const F_CR2,
    dr6 = const F_DR6,
    cs = const F_CS,
    pf = const vectors::PF,
    db = const vectors::DB,
    df = const vectors::DF,
    gs_base = const x86::IA32_GS_BASE,
    dr6_idle = const vectors::DR6_RESET,
    ksp = const offset_of!(PerCpu, kernel_rsp0),
    frame_bytes = const size_of::<TrapFrame>(),
    frame_words = const size_of::<TrapFrame>() / 8,
    debug = const cfg!(debug_assertions) as u8,
    dispatch = sym trap_dispatch,
);

/// Registered bodies, as `TrapBody` addresses; 0 runs [`default_body`].
static BODIES: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];

/// Called by both entry paths with the frame they built.
///
/// # Safety
/// `frame` points at the `TrapFrame` the calling stub built on this CPU's
/// stack, and nothing else refers to it for the call.
unsafe extern "C" fn trap_dispatch(frame: *mut TrapFrame) {
    // SAFETY: invariant: `frame` is the calling entry path's own frame on
    // this stack, which nothing else refers to during the call; established
    // by the entry paths (`vibeos_trap_entry`, `vibeos_trap_entry_ist`) that
    // `arch::x86_64::idt::init` points every gate at.
    let frame = unsafe { &mut *frame };
    let v = frame.vector as u8;
    // Before anything that reads `gs:`, `catch::intercept` included: a
    // fault on a return-to-user `iretq` arrives on the user GS.
    if matches!(v, vectors::NP | vectors::SS | vectors::GP) {
        user_return_fault(frame);
    }
    // The irqoff tracer: an entry that interrupted IF=1 code opens a
    // stretch at this vector. After `user_return_fault`, which runs on the
    // user GS; its frame (an exit `iretq`) has IF=0 anyway.
    if frame.iret.rflags & RFLAGS_IF != 0 {
        crate::sched::irqoff::off(crate::sched::irqoff::Site::vector(v));
    }
    #[cfg(feature = "kernel_tests")]
    if frame.user_mode() {
        testing::on_cpl3_entry(frame);
    }
    vibeos::log::trace::trap_enter(v, frame.cr2, frame.error_code);
    if !pre_body(frame, v) {
        // A fault or trap taken at CPL 3 runs its body with IF=1 (DESIGN
        // §2.9 rule 3): the stub has saved the frame, CR2 included. IST
        // vectors and IRQ top halves keep IF=0.
        if cpl3_body_if_on(v) && frame.user_mode() {
            x86::sti();
        }
        let p = BODIES[v as usize].load(Ordering::Acquire);
        if p == 0 {
            default_body(frame);
        } else {
            // SAFETY: invariant: a nonzero `BODIES` entry is a `TrapBody`
            // address; established by `arch::idt::set_handler`, its only store.
            let body: TrapBody = unsafe { core::mem::transmute::<usize, TrapBody>(p) };
            body(frame);
        }
    }
    vibeos::log::trace::trap_exit(v);
    if frame.user_mode() {
        exit_to_user(frame);
    }
    // The stub's `iretq` returns to IF=1.
    if frame.iret.rflags & RFLAGS_IF != 0 {
        crate::sched::irqoff::on();
    }
}

/// RFLAGS.IF.
const RFLAGS_IF: u64 = 1 << 9;

/// Vectors 0 to 31 that do not enter through an IST stack, and `#DB`,
/// whose CPL-3 frame the stub has moved off its IST stack. NMI and `#MC`
/// keep IF=0.
fn cpl3_body_if_on(v: u8) -> bool {
    v < 32 && ((PARANOID_MASK >> v) & 1 == 0 || v == vectors::DB)
}

/// What a ring-3 fault runs before the kernel-fault path: the process
/// layer's `try_user_fault`, which signals the faulting process and does
/// not return, or returns when no process owns the fault. `proc_init::init`
/// sets it before the first ring-3 entry (DESIGN §1.2). Unset, it returns.
static USER_FAULT: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the ring-3 fault hook.
pub fn set_user_fault_hook(f: fn(&TrapFrame)) {
    // Release: pairs with the Acquire load in `user_fault`.
    USER_FAULT.store(f as *mut (), Ordering::Release);
}

/// Run the ring-3 fault hook. The syscall exit's non-canonical-RIP path
/// takes it too.
pub fn user_fault(frame: &TrapFrame) {
    // Acquire: pairs with the Release store in `set_user_fault_hook`.
    let p = USER_FAULT.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `USER_FAULT` holds a `fn(&TrapFrame)`;
    // established by `arch::idt::set_user_fault_hook`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn(&TrapFrame)>(p) };
    f(frame);
}

/// The exception intercept for vectors 0 to 31: `catch::intercept`, which
/// `catch::init` sets right after `idt::init` (DESIGN §1.2). Unset, no
/// exception is intercepted. `kernel_tests` only (ROADMAP §10.2, F146).
#[cfg(feature = "kernel_tests")]
static INTERCEPT: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the exception intercept. `true` from it skips the body.
#[cfg(feature = "kernel_tests")]
pub fn set_intercept_hook(f: fn(&mut TrapFrame) -> bool) {
    // Release: pairs with the Acquire load in `intercept`.
    INTERCEPT.store(f as *mut (), Ordering::Release);
}

#[cfg(feature = "kernel_tests")]
#[inline(always)]
fn intercept(frame: &mut TrapFrame) -> bool {
    // Acquire: pairs with the Release store in `set_intercept_hook`.
    let p = INTERCEPT.load(Ordering::Acquire);
    if p.is_null() {
        return false;
    }
    // SAFETY: invariant: a non-null `INTERCEPT` holds a
    // `fn(&mut TrapFrame) -> bool`; established by
    // `arch::idt::set_intercept_hook`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn(&mut TrapFrame) -> bool>(p) };
    f(frame)
}

/// Test hook, then the intercept (`catch::intercept`). `true` skips the
/// body. Both exist only with `kernel_tests`; a production build runs
/// every body.
#[inline(always)]
fn pre_body(frame: &mut TrapFrame, v: u8) -> bool {
    #[cfg(feature = "kernel_tests")]
    {
        testing::on_entry(frame, v) || (v < 32 && intercept(frame))
    }
    #[cfg(not(feature = "kernel_tests"))]
    {
        let _ = (frame, v);
        false
    }
}

// Labels on the `iretq` instructions that return to ring 3.
unsafe extern "C" {
    static vibeos_syscall_iretq: u8;
    static vibeos_trap_iret: u8;
    static vibeos_trap_iret_ist: u8;
}

/// Whether `rip` is one of the labeled `iretq` instructions that return
/// to ring 3: the syscall slow path, which a new thread's first return
/// takes too, and both vector exits.
fn is_user_return_iretq(rip: u64) -> bool {
    [
        (&raw const vibeos_syscall_iretq) as u64,
        (&raw const vibeos_trap_iret) as u64,
        (&raw const vibeos_trap_iret_ist) as u64,
    ]
    .contains(&rip)
}

/// A `#GP`, `#NP`, or `#SS` raised by a return-to-user `iretq` (a
/// non-canonical RIP, a bad selector) arrives with the kernel CS and,
/// after the exit's `swapgs`, the user GS base. Move to the kernel GS if
/// `GS_BASE` is not a kernel (negative) address, then kill the process
/// with `SIGSEGV` whatever the vector (DESIGN §5.2, §5.10 rule 2). Returns
/// when the fault is anything else.
#[allow(
    clippy::panic,
    reason = "kernel invariant (DESIGN §9.4): a user-return iretq runs only for a thread with a process, so `user_fault` does not return"
)]
fn user_return_fault(frame: &TrapFrame) {
    if gs::from_user(frame.iret.cs) || !is_user_return_iretq(frame.iret.rip) {
        return;
    }
    // SAFETY: invariant: a CPL-0 fault whose RIP is a labeled user-return
    // `iretq` left RSP at that `iretq`'s five-word frame on this CPU's
    // kernel stack; established at `arch::x86_64::idt::is_user_return_iretq`,
    // whose labels each sit on such an `iretq`.
    let user = unsafe { ptr::read(frame.iret.rsp as *const InterruptFrame) };
    if !gs::from_user(user.cs) {
        return;
    }
    if (x86::rdmsr(x86::IA32_GS_BASE) as i64) >= 0 {
        // The exit's `swapgs` left this CPU's `PerCpu` in KERNEL_GS_BASE.
        // SAFETY: invariant: KERNEL_GS_BASE holds this CPU's `PerCpu`
        // from the exit's `swapgs` (or `syscall_init::first_return`'s write)
        // until `iretq` completes; established by `syscall_init`'s exits
        // and `arch::idt::vibeos_trap_entry`.
        unsafe { x86::wrmsr(x86::IA32_GS_BASE, x86::rdmsr(x86::IA32_KERNEL_GS_BASE)) };
    }
    let ctx = UserFrame {
        rip: user.rip,
        cs: user.cs,
        rflags: user.rflags,
        rsp: user.rsp,
        ss: user.ss,
        ..UserFrame::zeroed()
    };
    user_fault(&TrapFrame::for_user(vectors::GP, frame.error_code, &ctx));
    panic!(
        "idt: user-return iretq fault with no process, rip={:#x}",
        user.rip
    );
}

/// What every vector's return to ring 3 runs with IF=0: the exit work
/// (DESIGN §5.10 rule 11) and the FP binding check (DESIGN §7.5), which
/// `syscall_init::init_bsp` sets before the first ring-3 entry (DESIGN
/// §1.2) as `syscall_init::user_return`. Unset, nothing is checked.
static USER_RETURN: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the user-return hook.
pub fn set_user_return_hook(f: fn(&mut TrapFrame)) {
    // Release: pairs with the Acquire load in `exit_to_user`.
    USER_RETURN.store(f as *mut (), Ordering::Release);
}

/// The last step before the stub's exit for a frame whose saved CS.RPL is
/// 3. Only the dispatcher calls it.
pub fn exit_to_user(frame: &mut TrapFrame) {
    x86::cli();
    // Acquire: pairs with the Release store in `set_user_return_hook`.
    let p = USER_RETURN.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `USER_RETURN` holds a
    // `fn(&mut TrapFrame)`; established by `arch::idt::set_user_return_hook`,
    // its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn(&mut TrapFrame)>(p) };
    f(frame);
}

/// Whether the vector table gives `v` a fixed owner: the LAPIC LVT
/// vectors, the IPIs and the spurious vector, whose bodies `init` installs.
pub const fn fixed_owner(v: u8) -> bool {
    matches!(
        v,
        vectors::LAPIC_TIMER
            | vectors::LAPIC_ERROR
            | vectors::LAPIC_THERMAL
            | vectors::IPI_CALL
            | vectors::IPI_SHOOTDOWN
            | vectors::IPI_RESCHEDULE
            | vectors::IPI_HALT
            | vectors::LAPIC_SPURIOUS
    )
}

/// Whether [`set_handler`] accepts `v`: not an exception (0 to 31) and not
/// a vector with a fixed owner.
pub const fn registrable(v: u8) -> bool {
    v >= 32 && !fixed_owner(v)
}

/// Run `body` for `vector`. Writes no gate: every gate already points at
/// its stub. Exception and fixed-owner vectors are refused: their bodies
/// are the table's own, which `init` installs.
pub fn set_handler(vector: u8, body: TrapBody) {
    // First, before any guard, so a caught panic leaves nothing held. A
    // kernel invariant no input reaches (AGENTS.md rule 4, DESIGN §9.4):
    // callers pass vector constants or `irq_init`'s pool vectors.
    assert!(registrable(vector), "idt: set_handler on a reserved vector");
    install(vector, body);
}

/// Store `body` for `vector`, any vector. `init`'s table rows only.
fn install(vector: u8, body: TrapBody) {
    BODIES[vector as usize].store(body as usize, Ordering::Release);
}

fn default_body(frame: &mut TrapFrame) {
    let n = frame.vector as u8;
    let err = frame.error_code;
    let cr2 = (n == vectors::PF).then_some(frame.cr2);
    if frame.user_mode() {
        user_fault(frame);
    }
    let err = vectors::pushes_error_code(n).then_some(err);
    x86::cli();
    crate::panic::exception_vec(n, &frame.iret, frame.user().rbp, err, cr2);
}

fn breakpoint(frame: &mut TrapFrame) {
    if frame.user_mode() {
        user_fault(frame);
    }
    #[cfg(feature = "kernel_tests")]
    if !frame.user_mode() {
        testing::note_breakpoint();
    }
    dump(b"#BP", &frame.iret, None, None);
}

fn invalid_opcode(frame: &mut TrapFrame) {
    if frame.user_mode() {
        user_fault(frame);
    }
    x86::cli();
    crate::panic::exception_halt(b"#UD", &frame.iret, frame.user().rbp, None, None);
}

/// The interrupted registers a frame saved, for the stop primitive's
/// crash-register slot (DESIGN §2.5 step 1).
fn crash_regs(frame: &TrapFrame) -> vibeos::irq::stop::CrashRegs {
    vibeos::irq::stop::CrashRegs {
        rip: frame.iret.rip,
        rsp: frame.iret.rsp,
        rbp: frame.rbp,
        rflags: frame.iret.rflags,
    }
}

fn nmi(frame: &mut TrapFrame) {
    // The stop primitive decides first, before any write or lock: the
    // dump's owner may be inside `write_owner` (DESIGN §2.5 step 1). It
    // halts or stops this CPU, or returns `Return` on the owner.
    if crate::ipi_init::nmi_stop(crash_regs(frame)) == vibeos::irq::stop::NmiAction::Return {
        return;
    }
    // It runs inside whatever this CPU held: no lock (DESIGN §2.2).
    let _lockless = crate::sync_init::lockless_section();
    crate::panic::exception_halt(b"nmi", &frame.iret, frame.user().rbp, None, None);
}

/// A CPL-3 `#DB` (a single step, `int1`, a hardware breakpoint) kills the
/// process with `SIGTRAP`: its frame is on the thread's kernel stack and
/// its body runs with IF=1, so the kill path may block. The cause comes
/// from the DR6 the stub saved, never from the register (DESIGN §5.10
/// rule 9). Under `kernel_tests` a `testing` hook for `#DB` runs first, in
/// the dispatcher, and can resume instead.
fn debug_ex(frame: &mut TrapFrame) {
    if frame.user_mode() {
        #[cfg(feature = "kernel_tests")]
        if testing::on_user_db(frame) {
            return;
        }
        user_fault(frame);
    }
    // A CPL-0 `#DB` runs inside whatever this CPU held: no lock (DESIGN
    // §2.2).
    let _lockless = crate::sync_init::lockless_section();
    x86::cli();
    crate::panic::exception_halt(b"#DB", &frame.iret, frame.user().rbp, None, None);
}

/// A vector 11, 12 or 13 from ring 3 kills the process and does not
/// return; from the kernel it returns, and the caller halts.
fn kill_if_user(frame: &mut TrapFrame) {
    if frame.user_mode() {
        user_fault(frame);
    }
}

fn segment_not_present(frame: &mut TrapFrame) {
    kill_if_user(frame);
    x86::cli();
    crate::panic::exception_vec(
        vectors::NP,
        &frame.iret,
        frame.user().rbp,
        Some(frame.error_code),
        None,
    );
}

fn stack_fault(frame: &mut TrapFrame) {
    kill_if_user(frame);
    x86::cli();
    crate::panic::exception_vec(
        vectors::SS,
        &frame.iret,
        frame.user().rbp,
        Some(frame.error_code),
        None,
    );
}

fn general_protection(frame: &mut TrapFrame) {
    kill_if_user(frame);
    x86::cli();
    crate::panic::exception_halt(
        b"#GP",
        &frame.iret,
        frame.user().rbp,
        Some(frame.error_code),
        None,
    );
}

/// Reads CR2 from the frame: with IF=1 a preempting thread's fault can
/// change the register (DESIGN §5.10 rule 9). A CPL-0 fault on a user
/// accessor's copy with CR2 in the user half resumes at its exception-table
/// fixup, RCX, RSI and RDI as the fault left them, and the saved AC, which
/// the fixup's `clac` clears (INTERRUPTS §5.1).
fn page_fault(frame: &mut TrapFrame) {
    let (err, cr2) = (frame.error_code, frame.cr2);
    if frame.user_mode() {
        #[cfg(feature = "kernel_tests")]
        testing::on_user_pf(frame);
        user_fault(frame);
    } else if let Some(rip) = super::uaccess::fixup(frame.iret.rip, cr2) {
        frame.iret.rip = rip;
        return;
    }
    x86::cli();
    crate::panic::exception_halt(b"#PF", &frame.iret, frame.user().rbp, Some(err), Some(cr2));
}

fn double_fault(frame: &mut TrapFrame) {
    crate::panic::exception_halt(
        b"#DF",
        &frame.iret,
        frame.user().rbp,
        Some(frame.error_code),
        None,
    );
}

fn machine_check(frame: &mut TrapFrame) {
    // It runs inside whatever this CPU held: no lock (DESIGN §2.2).
    let _lockless = crate::sync_init::lockless_section();
    crate::panic::exception_halt(b"#MC", &frame.iret, frame.user().rbp, None, None);
}

fn pic_irq(frame: &mut TrapFrame) {
    pic::handle(frame.vector as u8);
}

fn pit_irq(_frame: &mut TrapFrame) {
    crate::time_init::on_pit_tick();
    crate::time_init::eoi_pit();
    crate::sched_init::on_timer_tick();
}

fn lapic_timer_irq(_frame: &mut TrapFrame) {
    crate::apic_init::on_timer_irq();
}

fn lapic_error_irq(_frame: &mut TrapFrame) {
    crate::apic_init::on_error_irq();
}

fn lapic_thermal_irq(_frame: &mut TrapFrame) {
    crate::apic_init::on_thermal_irq();
}

fn lapic_spurious_irq(_frame: &mut TrapFrame) {
    crate::apic_init::on_spurious_irq();
}

fn ipi_reschedule(_frame: &mut TrapFrame) {
    crate::apic_init::eoi();
    crate::ipi_init::on_reschedule_ipi();
}

fn ipi_shootdown(_frame: &mut TrapFrame) {
    crate::apic_init::eoi();
    crate::ipi_init::on_shootdown_ipi();
}

fn ipi_call(_frame: &mut TrapFrame) {
    crate::apic_init::eoi();
    crate::ipi_init::on_call_ipi();
}

fn ipi_halt(frame: &mut TrapFrame) {
    crate::apic_init::eoi();
    crate::ipi_init::on_stop_ipi(crash_regs(frame));
}

pub fn pointer() -> (u16, u64) {
    (
        (core::mem::size_of::<Idt>() - 1) as u16,
        IDT.as_ptr() as u64,
    )
}

/// # Safety
/// Shared IDT already filled. GDT loaded so KERNEL_CS and IST TSS match.
pub unsafe fn load() {
    let (limit, base) = pointer();
    let idtr = DtPtr { limit, base };
    // SAFETY: this fn's `# Safety` (here): `IDT` is the filled 256-entry
    // table, a `static` that never moves, and its gates name `KERNEL_CS`.
    unsafe { x86::lidt(&idtr) };
}

/// Point all 256 gates at their stubs, register the named bodies, `lidt`.
/// Each gate enters at the stub's `clac` when CPUID reports SMAP, the bit
/// `arch::cpu::init_control_regs` enables it from, and just past it otherwise.
/// This runs before that routine, and `clac` is legal once CPUID reports SMAP.
///
/// # Safety
/// GDT already loaded. PIC already remapped and masked.
pub unsafe fn init() {
    let base = (&raw const vibeos_trap_stubs).cast::<u8>();
    let skip = if crate::arch::cpu::cpuid_features().smap {
        0
    } else {
        CLAC.len()
    };
    IDT.with(|idt| {
        for (v, row) in ROWS.iter().enumerate() {
            let stub = base.wrapping_add(v * STUB_STRIDE);
            check_stub(stub, v, row);
            let gate = stub.wrapping_add(skip) as u64;
            idt.0[v] = IdtEntry::interrupt(gate, KERNEL_CS, row.ist, row.dpl);
        }
    });
    register_named();
    // SAFETY: `load`'s `# Safety`, established here: every gate was just
    // filled, and this fn's `# Safety` has the GDT loaded.
    unsafe { load() };
}

const _: () = {
    let mut v = 0;
    while v < 256 {
        assert!(ROWS[v].dpl == if v == vectors::BP as usize { 3 } else { 0 });
        v += 1;
    }
};

/// Kernel invariant: stub `v` starts with the bytes `ROWS[v]` asks for.
fn check_stub(stub: *const u8, v: usize, row: &Row) {
    let mut want = [0u8; 12];
    let mut n = 0;
    let mut put = |b: u8| {
        want[n] = b;
        n += 1;
    };
    for b in CLAC {
        put(b);
    }
    put(0xFC);
    if !row.err {
        put(0x6A);
        put(0x00);
    }
    if v < 128 {
        put(0x6A);
        put(v as u8);
    } else {
        put(0x68);
        put(v as u8);
    }
    // SAFETY: invariant: `vibeos_trap_stubs` is 256 strides of kernel text,
    // mapped readable for the kernel's life; established by the stub
    // `global_asm!` `arch::x86_64::idt::init` reads, in the kernel image.
    let got = unsafe { core::slice::from_raw_parts(stub, n) };
    assert!(got == &want[..n], "idt: stub {v} bytes");
}

fn register_named() {
    install(vectors::DB, debug_ex);
    install(vectors::NMI, nmi);
    install(vectors::BP, breakpoint);
    install(vectors::UD, invalid_opcode);
    install(vectors::DF, double_fault);
    install(vectors::NP, segment_not_present);
    install(vectors::SS, stack_fault);
    install(vectors::GP, general_protection);
    install(vectors::PF, page_fault);
    install(vectors::MC, machine_check);

    install(vectors::IRQ_PIT, pit_irq);
    for v in vectors::IRQ_BASE + 1..=vectors::IRQ_SPURIOUS_SLAVE {
        install(v, pic_irq);
    }

    install(vectors::LAPIC_TIMER, lapic_timer_irq);
    install(vectors::LAPIC_ERROR, lapic_error_irq);
    install(vectors::LAPIC_THERMAL, lapic_thermal_irq);
    install(vectors::LAPIC_SPURIOUS, lapic_spurious_irq);

    install(vectors::IPI_CALL, ipi_call);
    install(vectors::IPI_SHOOTDOWN, ipi_shootdown);
    install(vectors::IPI_RESCHEDULE, ipi_reschedule);
    install(vectors::IPI_HALT, ipi_halt);
}

const fn ist_for(vec: u8) -> u8 {
    match vec {
        vectors::DB => IstSlot::Debug.hardware(),
        vectors::NMI => IstSlot::Nmi.hardware(),
        vectors::DF => IstSlot::DoubleFault.hardware(),
        vectors::MC => IstSlot::MachineCheck.hardware(),
        _ => 0,
    }
}

/// One `vibeOS: <kind> rip=0x.. ..` line, in one write.
fn dump(kind: &[u8], frame: &InterruptFrame, err: Option<u64>, cr2: Option<u64>) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to Serial cannot fail (DESIGN §2.5)"
    )]
    let _ = serial::write_line_with(|w| {
        w.push_bytes(b"vibeOS: ");
        w.push_bytes(kind);
        crate::panic::frame_fields(w, frame, err, cr2);
        Ok(())
    });
}

/// In-guest test hooks. `kernel_tests` only (AGENTS.md rule 9).
#[cfg(feature = "kernel_tests")]
pub mod testing {
    use core::mem::size_of;
    use core::ptr;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

    use vibeos::syscall::UserFrame;
    use vibeos::vectors;

    use super::TrapFrame;

    /// Runs in the dispatcher before `catch::intercept` and the body;
    /// `true` skips both.
    pub type Hook = fn(&mut TrapFrame) -> bool;

    static HOOKS: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];

    /// Run `hook` on every entry of `vector` until it is cleared with `None`.
    pub fn set_hook(vector: u8, hook: Option<Hook>) {
        let p = hook.map_or(0, |h| h as usize);
        HOOKS[vector as usize].store(p, Ordering::Release);
    }
    /// CPL-0 `#BP`s whose body ran, past the catch intercept: the
    /// `int3_roundtrip` test reads it (ROADMAP §10.2, F142).
    static BP_HITS: AtomicU64 = AtomicU64::new(0);

    /// Count a CPL-0 `#BP`; the `#BP` body calls it.
    pub(super) fn note_breakpoint() {
        // Release: pairs with the Acquire load in `bp_hits`.
        BP_HITS.fetch_add(1, Ordering::Release);
    }

    /// CPL-0 `#BP` bodies run since boot.
    pub fn bp_hits() -> u64 {
        // Acquire: pairs with the Release add in `note_breakpoint`.
        BP_HITS.load(Ordering::Acquire)
    }

    static CPL3_HITS: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];

    /// Entries of `vector` whose saved CS.RPL was 3, since boot.
    pub fn cpl3_hits(vector: u8) -> u64 {
        CPL3_HITS[vector as usize].load(Ordering::Relaxed)
    }

    /// `cr2` of the fault whose `#PF` body yields once, at its top; 0 for none.
    static PF_YIELD_CR2: AtomicU64 = AtomicU64::new(0);
    static PF_YIELDING: AtomicBool = AtomicBool::new(false);
    static PF_DURING_YIELD: AtomicU64 = AtomicU64::new(0);

    /// The next CPL-3 `#PF` at `cr2` yields at the top of its body until
    /// another CPL-3 `#PF` body has run, for at most 1 s of TSC time.
    pub fn arm_pf_yield(cr2: u64) {
        PF_DURING_YIELD.store(0, Ordering::Release);
        PF_YIELDING.store(false, Ordering::Release);
        PF_YIELD_CR2.store(cr2, Ordering::Release);
    }

    pub fn disarm_pf_yield() {
        PF_YIELD_CR2.store(0, Ordering::Release);
    }

    /// The `cr2` of the first CPL-3 `#PF` body that ran during the yield;
    /// 0 for none.
    pub fn pf_during_yield() -> u64 {
        PF_DURING_YIELD.load(Ordering::Acquire)
    }

    /// Top of a CPL-3 `#PF` body, after the dispatcher's `sti`.
    pub(super) fn on_user_pf(frame: &TrapFrame) {
        if PF_YIELDING.load(Ordering::Acquire) {
            if PF_DURING_YIELD.load(Ordering::Acquire) == 0 {
                PF_DURING_YIELD.store(frame.cr2, Ordering::Release);
            }
            return;
        }
        let armed = PF_YIELD_CR2.load(Ordering::Acquire);
        if armed == 0
            || frame.cr2 != armed
            || PF_YIELD_CR2
                .compare_exchange(armed, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        PF_YIELDING.store(true, Ordering::Release);
        let t0 = crate::time_init::now_ns();
        while PF_DURING_YIELD.load(Ordering::Acquire) == 0
            && crate::time_init::now_ns().saturating_sub(t0) < 1_000_000_000
        {
            crate::thread_init::yield_now();
        }
        PF_YIELDING.store(false, Ordering::Release);
    }

    /// The 15 GPR canaries of `arm_canaries`, in `UserFrame` order
    /// (`r15` to `rdi`): `0xC0DE_0000_0000_0000 | i << 8 | i`, `i` from 1.
    pub const GPR_CANARIES: [u64; 15] = {
        let mut c = [0u64; 15];
        let mut i = 0;
        while i < 15 {
            let n = (i + 1) as u64;
            c[i] = 0xC0DE_0000_0000_0000 | (n << 8) | n;
            i += 1;
        }
        c
    };

    /// The canary hook's code range `[lo, hi)`, 0 when disarmed.
    static CANARY_LO: AtomicU64 = AtomicU64::new(0);
    static CANARY_HI: AtomicU64 = AtomicU64::new(0);
    static CANARY_EXIT: AtomicU64 = AtomicU64::new(0);
    static CANARY_TARGET: AtomicU64 = AtomicU64::new(0);
    static CANARY_HITS: AtomicU64 = AtomicU64::new(0);
    static CANARY_BAD: AtomicU64 = AtomicU64::new(0);
    static CANARY_MISPLACED: AtomicU64 = AtomicU64::new(0);

    /// For each CPL-3 `IPI_RESCHEDULE` entry whose saved RIP is in
    /// `[lo, hi)`, check that the stub's user frame sits at the top of the
    /// current thread's kernel stack and holds [`GPR_CANARIES`]; at the
    /// `target`th good hit, send the frame's RIP to `exit_va`.
    pub fn arm_canaries(lo: u64, hi: u64, exit_va: u64, target: u64) {
        CANARY_HITS.store(0, Ordering::Release);
        CANARY_BAD.store(0, Ordering::Release);
        CANARY_MISPLACED.store(0, Ordering::Release);
        CANARY_EXIT.store(exit_va, Ordering::Release);
        CANARY_TARGET.store(target, Ordering::Release);
        CANARY_HI.store(hi, Ordering::Release);
        CANARY_LO.store(lo, Ordering::Release);
    }

    pub fn disarm_canaries() {
        CANARY_LO.store(0, Ordering::Release);
        CANARY_HI.store(0, Ordering::Release);
    }

    pub fn hits() -> u64 {
        CANARY_HITS.load(Ordering::Acquire)
    }

    pub fn bad() -> u64 {
        CANARY_BAD.load(Ordering::Acquire)
    }

    pub fn misplaced() -> u64 {
        CANARY_MISPLACED.load(Ordering::Acquire)
    }

    /// Every CPL-3 entry, before the body.
    pub(super) fn on_cpl3_entry(frame: &mut TrapFrame) {
        let lo = CANARY_LO.load(Ordering::Acquire);
        let rip = frame.iret.rip;
        if frame.vector != u64::from(vectors::IPI_RESCHEDULE)
            || lo == 0
            || rip < lo
            || rip >= CANARY_HI.load(Ordering::Acquire)
        {
            return;
        }
        let t = crate::per_cpu_init::current_thread();
        // SAFETY: invariant: a non-null current thread is this CPU's live
        // TCB while it runs, and IF=0 keeps it current; established by
        // `per_cpu_init::set_current_thread`.
        let top = (!t.is_null())
            .then(|| unsafe { (*t).stack.as_ref().map(|s| s.top().as_u64()) })
            .flatten();
        let at = ptr::from_ref(frame.user()) as u64;
        if top.is_none_or(|top| at != top - size_of::<UserFrame>() as u64) {
            CANARY_MISPLACED.fetch_add(1, Ordering::AcqRel);
            return;
        }
        let u = frame.user();
        let got = [
            u.r15, u.r14, u.r13, u.r12, u.rbp, u.rbx, u.r11, u.r10, u.r9, u.r8, u.rax, u.rcx,
            u.rdx, u.rsi, u.rdi,
        ];
        if got != GPR_CANARIES {
            CANARY_BAD.fetch_add(1, Ordering::AcqRel);
            return;
        }
        let hits = CANARY_HITS.fetch_add(1, Ordering::AcqRel) + 1;
        if hits == CANARY_TARGET.load(Ordering::Acquire) {
            frame.user_mut().rip = CANARY_EXIT.load(Ordering::Acquire);
        }
    }

    /// The one-shot `#DB` re-pin hook's code range `[lo, hi)`; 0 when
    /// disarmed or spent.
    static REPIN_LO: AtomicU64 = AtomicU64::new(0);
    static REPIN_HI: AtomicU64 = AtomicU64::new(0);
    static REPIN_FROM: AtomicU32 = AtomicU32::new(u32::MAX);
    static REPIN_TO: AtomicU32 = AtomicU32::new(u32::MAX);

    /// The next CPL-3 `#DB` whose saved RIP is in `[lo, hi)`, at the top
    /// of its body: yield with the C-REQUEUE-HOOK on, so the thread moves
    /// to another CPU, clear TF in the frame, and resume the program
    /// instead of killing it.
    pub fn arm_db_repin(lo: u64, hi: u64) {
        REPIN_FROM.store(u32::MAX, Ordering::Release);
        REPIN_TO.store(u32::MAX, Ordering::Release);
        REPIN_HI.store(hi, Ordering::Release);
        REPIN_LO.store(lo, Ordering::Release);
    }

    pub fn disarm_db_repin() {
        REPIN_LO.store(0, Ordering::Release);
    }

    /// The CPU the re-pinned `#DB` body started on and the one it resumed
    /// on; `u32::MAX` for none.
    pub fn repin_cpus() -> (u32, u32) {
        (
            REPIN_FROM.load(Ordering::Acquire),
            REPIN_TO.load(Ordering::Acquire),
        )
    }

    /// Top of a CPL-3 `#DB` body, IF=1. `true`: resume the program.
    pub(super) fn on_user_db(frame: &mut TrapFrame) -> bool {
        let lo = REPIN_LO.load(Ordering::Acquire);
        let rip = frame.user().rip;
        if lo == 0
            || rip < lo
            || rip >= REPIN_HI.load(Ordering::Acquire)
            || REPIN_LO
                .compare_exchange(lo, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        // IF off from the store of the first CPU to the hook's end: a tick
        // before the yield, or on the new CPU before the hook is off, would
        // preempt the thread with the hook on and move it again, back to
        // CPU 0 at `-smp 2`. The yield switches away with IF off, as
        // `thread_exit` does.
        let _irq = crate::x86::InterruptGuard::enter();
        REPIN_FROM.store(crate::thread_init::current_cpu(), Ordering::Release);
        crate::sched::ktest::set_requeue_next_cpu(true);
        crate::thread_init::yield_now();
        crate::sched::ktest::set_requeue_next_cpu(false);
        REPIN_TO.store(crate::thread_init::current_cpu(), Ordering::Release);
        frame.user_mut().rflags &= !RFLAGS_TF;
        true
    }

    const RFLAGS_TF: u64 = 1 << 8;

    pub(super) fn on_entry(frame: &mut TrapFrame, v: u8) -> bool {
        if frame.user_mode() {
            CPL3_HITS[v as usize].fetch_add(1, Ordering::Relaxed);
        }
        let p = HOOKS[v as usize].load(Ordering::Acquire);
        if p == 0 {
            return false;
        }
        // SAFETY: invariant: a nonzero `HOOKS` entry is a `Hook` address;
        // established here, in this module's `set_hook`, its only store.
        let hook: Hook = unsafe { core::mem::transmute::<usize, Hook>(p) };
        hook(frame)
    }
}
