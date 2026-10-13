//! Sixteen-entry VBAR: early dump-and-halt, then the full table.
//!
//! Each synchronous entry saves ESR and FAR before it unmasks DAIF
//! (DESIGN §5.10 rule 9). The stack-bit test runs before the first store
//! (DESIGN §11.5).

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use crate::arch::current::UserFrame;
use vibeos::arch::aarch64::trap::{
    self, SLOT_CURRENT_SPX_IRQ, SLOT_LOWER_A64_IRQ, SLOT_LOWER_A64_SYNC,
};
use vibeos::kalloc::TryBox;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::proc::uaccess::untag_user_addr;
use vibeos::trap::{Ring3Action, TrapKind, ring3_action};

use super::cpu;
use super::percpu::CURRENT_OFFSET;
use crate::kva_init;
use crate::kva_init::GuardedStack;

/// Saved EL1 frame: x0-x30, SP, ELR, SPSR, ESR, FAR, slot. 304 bytes.
pub const FRAME_SIZE: u64 = 304;
/// DESIGN §5.10 user frame at the top of the thread's kernel stack.
pub const USER_FRAME_SIZE: u64 = 288;
const STACK_BIT: u32 = 14;
const _: () = assert!(core::mem::size_of::<UserFrame>() == USER_FRAME_SIZE as usize);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp: u64,
    pub elr: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
    pub slot: u64,
    pub _pad: u64,
}

impl TrapFrame {
    #[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
    pub fn for_user(_vec: u8, _err: u64, _f: &UserFrame) -> Self {
        Self {
            x: [0; 31],
            sp: 0,
            elr: 0,
            spsr: 0,
            esr: 0,
            far: 0,
            slot: 0,
            _pad: 0,
        }
    }
}

const _: () = assert!(core::mem::size_of::<TrapFrame>() == FRAME_SIZE as usize);
const _: () = assert!(core::mem::offset_of!(TrapFrame, x) + 30 * 8 == 240);

static FULL: AtomicBool = AtomicBool::new(false);
static OVERFLOW: AtomicPtr<GuardedStack> = AtomicPtr::new(core::ptr::null_mut());

/// Overflow stack top. The vector stub loads this before any store.
#[unsafe(no_mangle)]
static VIBEOS_OVERFLOW_SP: AtomicU64 = AtomicU64::new(0);

type Body = fn(&mut TrapFrame);
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
static USER_FAULT: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
static USER_RETURN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
static INTERCEPT: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

unsafe extern "C" {
    fn vibeos_early_vectors();
    fn vibeos_vectors();
}

global_asm!(
    ".pushsection .text",
    ".balign 0x800",
    ".global vibeos_early_vectors",
    "vibeos_early_vectors:",
    ".rept 16",
    "    b vibeos_early_entry",
    "    .balign 0x80",
    ".endr",
    "vibeos_early_entry:",
    "    mov x0, sp",
    "    mrs x1, elr_el1",
    "    mrs x2, esr_el1",
    "    mrs x3, far_el1",
    "    b vibeos_early_rust",
    ".popsection",
);

global_asm!(
    ".pushsection .text",
    ".balign 0x800",
    ".global vibeos_vectors",
    "vibeos_vectors:",
    // Exchange SP and x0 so the stack-bit test needs no scratch
    // (DESIGN §11.5 rule 6). LLVM IAS rejects `add x0, x0, sp` and
    // `sub sp, x0, sp`; this order uses SP only as Rn/Rd.
    // Overflow stashes x0 in TPIDR_EL0 and SP in TPIDRRO_EL0.
    ".macro vibeos_swap_sp_x0",
    "    sub x0, sp, x0",
    "    sub sp, sp, x0",
    "    add x0, sp, x0",
    ".endm",
    ".macro vibeos_slot_el1 n",
    "    vibeos_swap_sp_x0",
    "    sub x0, x0, #{frame}",
    "    tbnz x0, #{sbit}, 1f",
    "    vibeos_swap_sp_x0",
    "    stp x0, x1, [sp, #0]",
    "    mov x0, \\n",
    "    b vibeos_save_el1",
    "1:",
    "    add x0, x0, #{frame}",
    "    vibeos_swap_sp_x0",
    "    msr tpidr_el0, x0",
    "    mov x0, sp",
    "    msr tpidrro_el0, x0",
    "    mov x0, \\n",
    "    b vibeos_overflow",
    "    .balign 0x80",
    ".endm",
    ".macro vibeos_slot_el0 n",
    "    vibeos_swap_sp_x0",
    "    sub x0, x0, #{uframe}",
    "    tbnz x0, #{sbit}, 1f",
    "    vibeos_swap_sp_x0",
    "    stp x0, x1, [sp, #0]",
    "    mov x0, \\n",
    "    b vibeos_save_el0",
    "1:",
    "    add x0, x0, #{uframe}",
    "    vibeos_swap_sp_x0",
    "    msr tpidr_el0, x0",
    "    mov x0, sp",
    "    msr tpidrro_el0, x0",
    "    mov x0, \\n",
    "    b vibeos_overflow",
    "    .balign 0x80",
    ".endm",
    "    vibeos_slot_el1 0",
    "    vibeos_slot_el1 1",
    "    vibeos_slot_el1 2",
    "    vibeos_slot_el1 3",
    "    vibeos_slot_el1 4",
    "    vibeos_slot_el1 5",
    "    vibeos_slot_el1 6",
    "    vibeos_slot_el1 7",
    "    vibeos_slot_el0 8",
    "    vibeos_slot_el0 9",
    "    vibeos_slot_el0 10",
    "    vibeos_slot_el0 11",
    "    vibeos_slot_el1 12",
    "    vibeos_slot_el1 13",
    "    vibeos_slot_el1 14",
    "    vibeos_slot_el1 15",
    "vibeos_save_el1:",
    "    stp x2, x3, [sp, #16]",
    "    stp x4, x5, [sp, #32]",
    "    stp x6, x7, [sp, #48]",
    "    stp x8, x9, [sp, #64]",
    "    stp x10, x11, [sp, #80]",
    "    stp x12, x13, [sp, #96]",
    "    stp x14, x15, [sp, #112]",
    "    stp x16, x17, [sp, #128]",
    "    stp x18, x19, [sp, #144]",
    "    stp x20, x21, [sp, #160]",
    "    stp x22, x23, [sp, #176]",
    "    stp x24, x25, [sp, #192]",
    "    stp x26, x27, [sp, #208]",
    "    stp x28, x29, [sp, #224]",
    "    str x30, [sp, #240]",
    "    mov x1, sp",
    "    add x1, x1, #{frame}",
    "    str x1, [sp, #248]",
    "    mrs x1, elr_el1",
    "    mrs x2, spsr_el1",
    "    mrs x3, esr_el1",
    "    mrs x4, far_el1",
    "    stp x1, x2, [sp, #256]",
    "    stp x3, x4, [sp, #272]",
    "    str x0, [sp, #288]",
    "    mov x0, sp",
    "    bl vibeos_vector_rust",
    "    ldp x2, x3, [sp, #16]",
    "    ldp x4, x5, [sp, #32]",
    "    ldp x6, x7, [sp, #48]",
    "    ldp x8, x9, [sp, #64]",
    "    ldp x10, x11, [sp, #80]",
    "    ldp x12, x13, [sp, #96]",
    "    ldp x14, x15, [sp, #112]",
    "    ldp x16, x17, [sp, #128]",
    "    ldp x18, x19, [sp, #144]",
    "    ldp x20, x21, [sp, #160]",
    "    ldp x22, x23, [sp, #176]",
    "    ldp x24, x25, [sp, #192]",
    "    ldp x26, x27, [sp, #208]",
    "    ldp x28, x29, [sp, #224]",
    "    ldr x30, [sp, #240]",
    "    ldp x0, x1, [sp, #0]",
    "    add sp, sp, #{frame}",
    "    eret",
    "vibeos_save_el0:",
    "    stp x2, x3, [sp, #16]",
    "    stp x4, x5, [sp, #32]",
    "    stp x6, x7, [sp, #48]",
    "    stp x8, x9, [sp, #64]",
    "    stp x10, x11, [sp, #80]",
    "    stp x12, x13, [sp, #96]",
    "    stp x14, x15, [sp, #112]",
    "    stp x16, x17, [sp, #128]",
    "    stp x18, x19, [sp, #144]",
    "    stp x20, x21, [sp, #160]",
    "    stp x22, x23, [sp, #176]",
    "    stp x24, x25, [sp, #192]",
    "    stp x26, x27, [sp, #208]",
    "    stp x28, x29, [sp, #224]",
    "    str x30, [sp, #240]",
    "    mov x19, x0",
    "    mrs x1, sp_el0",
    "    str x1, [sp, #248]",
    "    mrs x1, elr_el1",
    "    mrs x2, spsr_el1",
    "    stp x1, x2, [sp, #256]",
    "    str xzr, [sp, #272]",
    "    mov x1, #-1",
    "    str x1, [sp, #280]",
    "    adrp x1, VIBEOS_TPIDR_EL2",
    "    add x1, x1, :lo12:VIBEOS_TPIDR_EL2",
    "    ldr x1, [x1]",
    "    cbnz x1, 20f",
    "    mrs x1, tpidr_el1",
    "    b 21f",
    "20:",
    "    mrs x1, tpidr_el2",
    "21:",
    "    cbz x1, 22f",
    "    ldr x1, [x1, #{cur}]",
    "22:",
    "    msr sp_el0, x1",
    "    mrs x2, esr_el1",
    "    mrs x3, far_el1",
    "    mov x0, sp",
    "    mov x1, x19",
    "    bl vibeos_el0_rust",
    "    b vibeos_el0_return",
    ".global vibeos_el0_return",
    "vibeos_el0_return:",
    // `daifset` only sets bits. While the exit breakpoint is armed, clear
    // D first so a mutant that writes `#2` leaves D clear at the `eret`.
    // Unarmed returns keep the mask the body already set.
    ".if {ktest}",
    "    adrp x1, VIBEOS_ERET_BP_ARMED",
    "    add x1, x1, :lo12:VIBEOS_ERET_BP_ARMED",
    "    ldarb w1, [x1]",
    "    cbz w1, 6f",
    "    msr daifclr, #8",
    "6:",
    ".endif",
    "    msr daifset, #0xf",
    ".if {debug}",
    "    mrs x1, daif",
    "    and x1, x1, #0x3c0",
    "    cmp x1, #0x3c0",
    "    b.eq 7f",
    // Armed: the hit count is the failure. Halting would hide it.
    ".if {ktest}",
    "    adrp x1, VIBEOS_ERET_BP_ARMED",
    "    add x1, x1, :lo12:VIBEOS_ERET_BP_ARMED",
    "    ldarb w1, [x1]",
    "    cbnz w1, 7f",
    ".endif",
    "    b vibeos_el0_daif_clear",
    "7:",
    ".endif",
    "    mov x0, sp",
    "    bl vibeos_el0_exit",
    "    ldp x2, x3, [sp, #16]",
    "    ldp x4, x5, [sp, #32]",
    "    ldp x6, x7, [sp, #48]",
    "    ldp x8, x9, [sp, #64]",
    "    ldp x10, x11, [sp, #80]",
    "    ldp x12, x13, [sp, #96]",
    "    ldp x14, x15, [sp, #112]",
    "    ldp x18, x19, [sp, #144]",
    "    ldp x20, x21, [sp, #160]",
    "    ldp x22, x23, [sp, #176]",
    "    ldp x24, x25, [sp, #192]",
    "    ldp x26, x27, [sp, #208]",
    "    ldp x28, x29, [sp, #224]",
    "    ldr x30, [sp, #240]",
    "    ldp x0, x1, [sp, #0]",
    ".if {debug}",
    "    mrs x16, daif",
    "    and x16, x16, #0x3c0",
    "    cmp x16, #0x3c0",
    "    b.eq 8f",
    ".if {ktest}",
    "    adrp x16, VIBEOS_ERET_BP_ARMED",
    "    add x16, x16, :lo12:VIBEOS_ERET_BP_ARMED",
    "    ldarb w16, [x16]",
    "    cbnz w16, 8f",
    ".endif",
    "    b vibeos_el0_daif_clear",
    "8:",
    ".endif",
    "    ldp x16, x17, [sp, #128]",
    "    add sp, sp, #{uframe}",
    ".global vibeos_el0_eret",
    "vibeos_el0_eret:",
    "    eret",
    "vibeos_el0_daif_clear:",
    "    b vibeos_el0_daif_halt",
    "vibeos_overflow:",
    "    adrp x1, VIBEOS_TPIDR_EL2",
    "    add x1, x1, :lo12:VIBEOS_TPIDR_EL2",
    "    ldr x1, [x1]",
    "    cbnz x1, 10f",
    "    mrs x1, tpidr_el1",
    "    b 11f",
    "10:",
    "    mrs x1, tpidr_el2",
    "11:",
    "    cbz x1, 12f",
    "    ldr x1, [x1, #{off}]",
    "    cbnz x1, 13f",
    "12:",
    "    adrp x1, VIBEOS_OVERFLOW_SP",
    "    add x1, x1, :lo12:VIBEOS_OVERFLOW_SP",
    "    ldr x1, [x1]",
    "    cbz x1, 2f",
    "13:",
    "    mov sp, x1",
    "2:",
    "    b vibeos_overflow_rust",
    ".popsection",
    frame = const FRAME_SIZE,
    uframe = const USER_FRAME_SIZE,
    sbit = const STACK_BIT,
    off = const crate::arch::aarch64::percpu::OVERFLOW_SP_OFFSET,
    cur = const CURRENT_OFFSET,
    debug = const cfg!(debug_assertions) as u32,
    ktest = const cfg!(feature = "kernel_tests") as u32,
);

/// Point VBAR at the early table. Safe before KVA.
pub fn init_early() {
    let v = vibeos_early_vectors as *const () as u64;
    write_vbar(v);
}

/// Full table, after the bootstrap thread is on a 2S guarded stack.
///
/// # Safety
/// KVA is up; this CPU runs on a guarded stack.
pub unsafe fn init_full() {
    let Ok(stack) = kva_init::alloc_guarded_stack(DEFAULT_STACK_PAGES) else {
        crate::boot::halt_with("vibeOS: vectors: no overflow stack");
    };
    let top = stack.top().as_u64();
    cpu::set_overflow_sp(top);
    // Release: pairs with the Acquire load in the overflow stub.
    VIBEOS_OVERFLOW_SP.store(top, Ordering::Release);
    match TryBox::try_new(stack) {
        Ok(b) => {
            let p = TryBox::into_raw(b);
            // Release: pairs with nothing; boot CPU only.
            OVERFLOW.store(p, Ordering::Release);
        }
        Err(_) => crate::boot::halt_with("vibeOS: vectors: overflow box"),
    }
    if crate::per_cpu_init::is_live() {
        crate::per_cpu_init::with_current(|c| c.overflow_sp = top);
    }
    write_vbar(vibeos_vectors as *const () as u64);
    // Release: pairs with the Acquire load in `full_live`.
    FULL.store(true, Ordering::Release);
    cpu::clear_pstate_a();
    crate::marker!("vibeOS: vectors ok");
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn full_live() -> bool {
    // Acquire: pairs with the Release store in `init_full`.
    FULL.load(Ordering::Acquire)
}

fn write_vbar(v: u64) {
    // SAFETY: `v` is a 2 KiB-aligned vector table in the kernel image. established here.
    unsafe {
        if cpu::el2_vhe() {
            asm!("msr vbar_el2, {0}", in(reg) v, options(nostack, preserves_flags));
        } else {
            asm!("msr vbar_el1, {0}", in(reg) v, options(nostack, preserves_flags));
        }
        asm!("isb", options(nostack, preserves_flags));
    }
}

#[unsafe(no_mangle)]
extern "C" fn vibeos_early_rust(sp: u64, elr: u64, esr: u64, far: u64) -> ! {
    crate::marker!(
        "vibeOS: panic: early sp={:#x} elr={:#x} esr={:#x} far={:#x}",
        sp,
        elr,
        esr,
        far
    );
    cpu::halt();
}

#[unsafe(no_mangle)]
extern "C" fn vibeos_overflow_rust(slot: u64) -> ! {
    let far: u64;
    let esr: u64;
    let elr: u64;
    let stash: u64;
    // SAFETY: TPIDRRO holds the stashed SP; ESR/FAR/ELR are the fault. established here.
    unsafe {
        asm!("mrs {0}, far_el1", out(reg) far, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, esr_el1", out(reg) esr, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, elr_el1", out(reg) elr, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, tpidrro_el0", out(reg) stash, options(nomem, nostack, preserves_flags));
    }
    let _ = slot;
    #[cfg(feature = "kernel_tests")]
    {
        let handler_sp: u64;
        // SAFETY: SP is this CPU's overflow stack. established here.
        unsafe {
            asm!("mov {0}, sp", out(reg) handler_sp, options(nomem, nostack, preserves_flags));
        }
        if intercept_overflow(far, esr, elr, handler_sp) {
            cpu::halt();
        }
    }
    let tid = crate::arch::current_tcb() as u64;
    crate::marker!(
        "vibeOS: panic: stack overflow far={:#x} esr={:#x} elr={:#x} sp={:#x} thread={:#x}",
        far,
        esr,
        elr,
        stash,
        tid
    );
    cpu::halt();
}

#[cfg(feature = "kernel_tests")]
fn intercept_overflow(far: u64, esr: u64, elr: u64, sp: u64) -> bool {
    super::catch::overflow(far, esr, elr, sp)
}

#[cfg(not(feature = "kernel_tests"))]
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
fn intercept_overflow(_far: u64, _esr: u64, _elr: u64, _sp: u64) -> bool {
    false
}

#[unsafe(no_mangle)]
extern "C" fn vibeos_vector_rust(frame: &mut TrapFrame) {
    cpu::daif_clear_da();
    let slot = frame.slot as u8;
    let kind = trap::decode(slot, frame.esr);
    match kind {
        TrapKind::Interrupt(_) => {
            crate::arch::aarch64::gic::handle_irq();
        }
        TrapKind::Fiq | TrapKind::SError => {
            crate::marker!(
                "vibeOS: panic: #{} esr={:#x} far={:#x} elr={:#x}",
                if matches!(kind, TrapKind::Fiq) {
                    "FIQ"
                } else {
                    "SERROR"
                },
                frame.esr,
                frame.far,
                frame.elr
            );
            cpu::halt();
        }
        _ => handle_sync(frame, kind),
    }
    restore(frame);
}

#[cfg(feature = "kernel_tests")]
fn on_eret_breakpoint(frame: &mut TrapFrame) {
    crate::syscall_init::testing::note_eret_bp();
    if crate::syscall_init::testing::take_probe() {
        // x30 is the probe continuation. The exception replaced ELR
        // with this `eret`, so returning there loops.
        frame.elr = frame.x[30];
        crate::syscall_init::testing::disarm_eret_breakpoint();
        return;
    }
    if let Some((pc, pstate)) = crate::syscall_init::testing::take_exit_target() {
        frame.elr = pc;
        frame.spsr = pstate;
        crate::syscall_init::testing::clear_eret_bcr();
        return;
    }
    // ELR is this `eret`. Returning there erets to itself.
    crate::syscall_init::testing::disarm_eret_breakpoint();
    crate::klog!(
        vibeos::log::Level::Error,
        "vibeOS: ktest: eret breakpoint with no target"
    );
    cpu::halt();
}

fn handle_sync(frame: &mut TrapFrame, kind: TrapKind) {
    #[cfg(feature = "kernel_tests")]
    if super::catch::intercept(frame) {
        return;
    }
    #[cfg(feature = "kernel_tests")]
    if matches!(kind, TrapKind::Debug(_)) && crate::syscall_init::testing::eret_breakpoint_armed() {
        on_eret_breakpoint(frame);
        return;
    }
    if let Some(fix) = super::uaccess::fixup(frame.elr, untag_user_addr(frame.far)) {
        frame.elr = fix;
        return;
    }
    match ring3_action(kind) {
        Ring3Action::NotRing3 | Ring3Action::Syscall | Ring3Action::StepOver => {
            if matches!(kind, TrapKind::WaitTrap) {
                frame.elr = frame.elr.wrapping_add(4);
                return;
            }
            crate::marker!(
                "vibeOS: panic: sync esr={:#x} far={:#x} elr={:#x} slot={}",
                frame.esr,
                frame.far,
                frame.elr,
                frame.slot
            );
            cpu::halt();
        }
        Ring3Action::Signal { .. } => {
            crate::marker!(
                "vibeOS: panic: #DABT esr={:#x} far={:#x} elr={:#x}",
                frame.esr,
                frame.far,
                frame.elr
            );
            cpu::halt();
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn vibeos_el0_rust(frame: &mut UserFrame, slot: u64, esr: u64, far: u64) {
    let far = untag_user_addr(far);
    let kind = trap::decode(slot as u8, esr);
    match kind {
        TrapKind::Interrupt(_) => {
            cpu::daif_clear_da();
            crate::arch::aarch64::gic::handle_irq();
            cpu::daif_set_all();
        }
        TrapKind::Fiq | TrapKind::SError => {
            crate::marker!(
                "vibeOS: panic: #{} esr={:#x} far={:#x} elr={:#x}",
                if matches!(kind, TrapKind::Fiq) {
                    "FIQ"
                } else {
                    "SERROR"
                },
                esr,
                far,
                frame.pc
            );
            cpu::halt();
        }
        TrapKind::Syscall => {
            cpu::daif_clear_all();
            crate::syscall_init::enter(frame);
            cpu::daif_set_all();
        }
        TrapKind::WaitTrap => {
            frame.pc = frame.pc.wrapping_add(4);
        }
        TrapKind::Debug(_) if slot as u8 == SLOT_LOWER_A64_SYNC => {
            #[cfg(feature = "kernel_tests")]
            if crate::syscall_init::testing::eret_breakpoint_armed() {
                crate::syscall_init::testing::note_eret_bp();
                return;
            }
            cpu::daif_clear_all();
            crate::proc_init::try_user_trap(kind, frame.pc, far);
            cpu::daif_set_all();
        }
        other => {
            cpu::daif_clear_all();
            crate::proc_init::try_user_trap(other, frame.pc, far);
            cpu::daif_set_all();
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn vibeos_el0_exit(frame: &mut UserFrame) {
    crate::syscall_init::exit_work(crate::syscall_init::EXIT_SYSCALL, frame);
    crate::syscall_init::vibeos_fp_user_return();
    #[cfg(feature = "kernel_tests")]
    if crate::syscall_init::testing::take_bad_elr() {
        frame.pc = 1u64 << 48;
    }
    #[cfg(feature = "kernel_tests")]
    if crate::syscall_init::testing::eret_breakpoint_armed() {
        crate::syscall_init::testing::note_exit_target(frame.pc, frame.pstate);
    }
    if !crate::syscall_init::elr_ok(frame.pc) {
        crate::proc_init::kill_bad_elr(frame.pc);
    }
    // SAFETY: `frame` is this thread's user frame at the top of its
    // kernel stack; DAIF is all set; established here by
    // `crate::arch::aarch64::vectors::vibeos_el0_return`.
    unsafe {
        core::arch::asm!(
            "msr elr_el1, {pc}",
            "msr spsr_el1, {pstate}",
            "msr sp_el0, {sp}",
            pc = in(reg) frame.pc,
            pstate = in(reg) frame.pstate,
            sp = in(reg) frame.sp,
            options(nostack, preserves_flags),
        );
    }
}

#[unsafe(no_mangle)]
extern "C" fn vibeos_el0_daif_halt() -> ! {
    crate::marker!("vibeOS: panic: el0 return with DAIF clear");
    cpu::halt();
}

fn restore(frame: &TrapFrame) {
    // SAFETY: `frame` is the live exception frame on this stack; ELR/SPSR
    // were saved before any unmask; established here.
    unsafe {
        asm!(
            "msr elr_el1, {elr}",
            "msr spsr_el1, {spsr}",
            elr = in(reg) frame.elr,
            spsr = in(reg) frame.spsr,
            options(nostack, preserves_flags),
        );
    }
}

pub fn set_handler(_v: u8, _body: Body) {}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn set_user_fault_hook(h: fn(&TrapFrame) -> bool) {
    // Release: pairs with the Acquire load in the fault path.
    USER_FAULT.store(h as *mut (), Ordering::Release);
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn set_user_return_hook(h: fn(&mut TrapFrame)) {
    // Release: pairs with the Acquire load on return-to-user.
    USER_RETURN.store(h as *mut (), Ordering::Release);
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn set_intercept_hook(h: fn(&mut TrapFrame) -> bool) {
    // Release: pairs with the Acquire load in `intercept`.
    INTERCEPT.store(h as *mut (), Ordering::Release);
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn user_fault(_frame: &TrapFrame) {}

/// Point VBAR at the full table. Secondaries call this; the overflow
/// stack lives in `PerCpu.overflow_sp`.
pub fn load() {
    write_vbar(vibeos_vectors as *const () as u64);
    cpu::clear_pstate_a();
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn registrable(_v: u8) -> bool {
    true
}

pub fn init() {
    // Full table waits for the bootstrap stack (`init_full`).
}

#[allow(dead_code)]
pub fn irq_slots() -> [u8; 2] {
    [SLOT_CURRENT_SPX_IRQ, SLOT_LOWER_A64_IRQ]
}
