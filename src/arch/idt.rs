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
use core::sync::atomic::{AtomicUsize, Ordering};

use vibeos::desc::{IdtEntry, InterruptFrame, IstSlot, KERNEL_CS};
use vibeos::fmt_util;
use vibeos::syscall::UserFrame;
use vibeos::vectors;

use crate::arch::catch;
use crate::arch::gs;
use crate::arch::pic;
use crate::cell::IrqCell;
use crate::serial::Serial;
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
    #[inline]
    pub fn user_mode(&self) -> bool {
        gs::from_user(self.iret.cs)
    }

    /// The 21 `user_regs_struct` words, `r15` to `ss`.
    #[allow(
        dead_code,
        reason = "C-TRAPFRAME accessor; P10-S21 builds the user frame on it"
    )]
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
    #[allow(
        dead_code,
        reason = "C-TRAPFRAME accessor; P10-S21 builds the user frame on it"
    )]
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
        err: false,
        ist: 0,
        dpl: 0,
        paranoid: false,
    }; 256];
    let mut v = 0;
    while v < 256 {
        let ist = ist_for(v as u8);
        rows[v] = Row {
            err: vectors::pushes_error_code(v as u8),
            ist,
            dpl: 0,
            paranoid: ist != 0,
        };
        v += 1;
    }
    rows
}

const ROWS: [Row; 256] = build_rows();

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

    // NMI, #DB, #DF, #MC. ebx (callee-saved) keeps the swap decision
    // across the call, and the exit mirrors it.
    .balign 16
    .global vibeos_trap_entry_ist
    vibeos_trap_entry_ist:
        vibeos_trap_save
        xor ebx, ebx
        test byte ptr [rsp + {cs}], 3
        jz 1f
        swapgs
        mov ebx, 1
    1:
        mov qword ptr [rsp + {cr2}], 0
        xor eax, eax
        cmp edi, {db}
        jne 2f
        mov rax, dr6
    2:
        mov [rsp + {dr6}], rax
        mov rdi, rsp
        call {dispatch}
        vibeos_trap_exit_cli
        test ebx, ebx
        jz 3f
        swapgs
    3:
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
    // by `arch::idt::vibeos_trap_entry` and `arch::idt::vibeos_trap_entry_ist`.
    let frame = unsafe { &mut *frame };
    let v = frame.vector as u8;
    if !pre_body(frame, v) {
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
    if frame.user_mode() {
        exit_to_user(frame);
    }
}

/// Test hook, then `catch::intercept`. `true` skips the body.
#[inline(always)]
fn pre_body(frame: &mut TrapFrame, v: u8) -> bool {
    #[cfg(feature = "kernel_tests")]
    if testing::on_entry(frame, v) {
        return true;
    }
    v < 32 && catch::intercept(frame)
}

/// The last step before the stub's exit for a frame whose saved CS.RPL is
/// 3. Only the dispatcher calls it.
pub fn exit_to_user(_frame: &mut TrapFrame) {}

/// Run `body` for `vector`. Writes no gate: every gate already points at
/// its stub.
pub fn set_handler(vector: u8, body: TrapBody) {
    BODIES[vector as usize].store(body as usize, Ordering::Release);
}

fn default_body(frame: &mut TrapFrame) {
    let n = frame.vector as u8;
    let err = frame.error_code;
    let cr2 = (n == vectors::PF).then_some(frame.cr2);
    if frame.user_mode() {
        crate::proc_init::try_user_fault(n, &frame.iret, err, cr2);
    }
    let err = vectors::pushes_error_code(n).then_some(err);
    crate::panic::exception_vec(n, &frame.iret, err, cr2);
}

fn breakpoint(frame: &mut TrapFrame) {
    dump(b"#BP", &frame.iret, None, None);
}

fn invalid_opcode(frame: &mut TrapFrame) {
    if frame.user_mode() {
        crate::proc_init::try_user_fault(vectors::UD, &frame.iret, 0, None);
    }
    crate::panic::exception_halt(b"#UD", &frame.iret, None, None);
}

fn nmi(frame: &mut TrapFrame) {
    crate::panic::exception_halt(b"nmi", &frame.iret, None, None);
}

fn debug_ex(frame: &mut TrapFrame) {
    crate::panic::exception_halt(b"#DB", &frame.iret, None, None);
}

/// `vector` 11, 12 or 13 from ring 3 kills the process and does not
/// return; from the kernel it returns, and the caller halts.
fn kill_if_user(frame: &mut TrapFrame, vector: u8) {
    let err = frame.error_code;
    if frame.user_mode() {
        crate::proc_init::try_user_fault(vector, &frame.iret, err, None);
    }
}

fn segment_not_present(frame: &mut TrapFrame) {
    kill_if_user(frame, vectors::NP);
    crate::panic::exception_vec(vectors::NP, &frame.iret, Some(frame.error_code), None);
}

fn stack_fault(frame: &mut TrapFrame) {
    kill_if_user(frame, vectors::SS);
    crate::panic::exception_vec(vectors::SS, &frame.iret, Some(frame.error_code), None);
}

fn general_protection(frame: &mut TrapFrame) {
    kill_if_user(frame, vectors::GP);
    crate::panic::exception_halt(b"#GP", &frame.iret, Some(frame.error_code), None);
}

fn page_fault(frame: &mut TrapFrame) {
    let (err, cr2) = (frame.error_code, frame.cr2);
    if frame.user_mode() {
        crate::proc_init::try_user_fault(vectors::PF, &frame.iret, err, Some(cr2));
    }
    crate::panic::exception_halt(b"#PF", &frame.iret, Some(err), Some(cr2));
}

fn double_fault(frame: &mut TrapFrame) {
    crate::panic::exception_halt(b"#DF", &frame.iret, Some(frame.error_code), None);
}

fn machine_check(frame: &mut TrapFrame) {
    crate::panic::exception_halt(b"#MC", &frame.iret, None, None);
}

fn pic_irq(frame: &mut TrapFrame) {
    pic::handle(frame.vector as u8);
}

fn pit_irq(_frame: &mut TrapFrame) {
    let tsc = crate::time_init::read_tsc();
    crate::time_init::on_pit_tick(tsc);
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

fn ipi_halt(_frame: &mut TrapFrame) {
    crate::apic_init::eoi();
    crate::ipi_init::on_halt_ipi();
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
    unsafe { x86::lidt(&idtr) };
}

/// Point all 256 gates at their stubs, register the named bodies, `lidt`.
/// Each gate enters at the stub's `clac` when CPUID reports SMAP, the bit
/// `arch::cpu::harden` enables it from, and just past it otherwise. This
/// runs before `harden`, and `clac` is legal once CPUID reports SMAP.
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
    unsafe { load() };
}

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
    // `global_asm!` in `arch::idt` and the kernel image mapping.
    let got = unsafe { core::slice::from_raw_parts(stub, n) };
    assert!(got == &want[..n], "idt: stub {v} bytes");
}

fn register_named() {
    set_handler(vectors::DB, debug_ex);
    set_handler(vectors::NMI, nmi);
    set_handler(vectors::BP, breakpoint);
    set_handler(vectors::UD, invalid_opcode);
    set_handler(vectors::DF, double_fault);
    set_handler(vectors::NP, segment_not_present);
    set_handler(vectors::SS, stack_fault);
    set_handler(vectors::GP, general_protection);
    set_handler(vectors::PF, page_fault);
    set_handler(vectors::MC, machine_check);

    set_handler(vectors::IRQ_PIT, pit_irq);
    for v in vectors::IRQ_BASE + 1..=vectors::IRQ_SPURIOUS_SLAVE {
        set_handler(v, pic_irq);
    }

    set_handler(vectors::LAPIC_TIMER, lapic_timer_irq);
    set_handler(vectors::LAPIC_ERROR, lapic_error_irq);
    set_handler(vectors::LAPIC_THERMAL, lapic_thermal_irq);
    set_handler(vectors::LAPIC_SPURIOUS, lapic_spurious_irq);

    set_handler(vectors::IPI_CALL, ipi_call);
    set_handler(vectors::IPI_SHOOTDOWN, ipi_shootdown);
    set_handler(vectors::IPI_RESCHEDULE, ipi_reschedule);
    set_handler(vectors::IPI_HALT, ipi_halt);
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

fn hex(n: u64) {
    let mut b = [0u8; 16];
    Serial::write_bytes(fmt_util::write_hex(n, &mut b));
}

fn dump(kind: &[u8], frame: &InterruptFrame, err: Option<u64>, cr2: Option<u64>) {
    Serial::write_bytes(b"vibeOS: ");
    Serial::write_bytes(kind);
    Serial::write_bytes(b" rip=0x");
    hex(frame.rip);
    Serial::write_bytes(b" cs=0x");
    hex(frame.cs);
    Serial::write_bytes(b" rflags=0x");
    hex(frame.rflags);
    Serial::write_bytes(b" rsp=0x");
    hex(frame.rsp);
    Serial::write_bytes(b" ss=0x");
    hex(frame.ss);
    if let Some(e) = err {
        Serial::write_bytes(b" err=0x");
        hex(e);
    }
    if let Some(c) = cr2 {
        Serial::write_bytes(b" cr2=0x");
        hex(c);
    }
    Serial::write_bytes(b"\n");
}

/// In-guest test hooks. `kernel_tests` only (AGENTS.md rule 9).
#[cfg(feature = "kernel_tests")]
pub mod testing {
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
    static CPL3_HITS: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];

    pub(super) fn on_entry(frame: &mut TrapFrame, v: u8) -> bool {
        if frame.user_mode() {
            CPL3_HITS[v as usize].fetch_add(1, Ordering::Relaxed);
        }
        let p = HOOKS[v as usize].load(Ordering::Acquire);
        if p == 0 {
            return false;
        }
        // SAFETY: invariant: a nonzero `HOOKS` entry is a `Hook` address;
        // established by `arch::idt::testing::set_hook`, its only store.
        let hook: Hook = unsafe { core::mem::transmute::<usize, Hook>(p) };
        hook(frame)
    }
}
