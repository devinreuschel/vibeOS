//! aarch64 context switch: x19-x29, SP, LR, DAIF (DESIGN §7.5).
//!
//! The kernel and `tests/hostlib`'s `switch_context` test both compile this
//! file. The includer defines `switch_cli!` and `switch_sti!` before its
//! `mod switch;`: DAIF writes in the kernel, and `""` on a host.

use vibeos::sched::thread::{CpuContext, DAIF_I};

core::arch::global_asm!(
    ".pushsection .text",
    ".global vibeos_switch_context",
    ".type vibeos_switch_context, @function",
    "vibeos_switch_context:",
    "    str x19, [x0, #{x19}]",
    "    str x20, [x0, #{x20}]",
    "    str x21, [x0, #{x21}]",
    "    str x22, [x0, #{x22}]",
    "    str x23, [x0, #{x23}]",
    "    str x24, [x0, #{x24}]",
    "    str x25, [x0, #{x25}]",
    "    str x26, [x0, #{x26}]",
    "    str x27, [x0, #{x27}]",
    "    str x28, [x0, #{x28}]",
    "    str x29, [x0, #{x29}]",
    "    mov x2, sp",
    "    str x2, [x0, #{sp}]",
    "    str x30, [x0, #{lr}]",
    switch_read_daif!(),
    "    str x2, [x0, #{daif}]",
    switch_cli!(),
    "    ldr x19, [x1, #{x19}]",
    "    ldr x20, [x1, #{x20}]",
    "    ldr x21, [x1, #{x21}]",
    "    ldr x22, [x1, #{x22}]",
    "    ldr x23, [x1, #{x23}]",
    "    ldr x24, [x1, #{x24}]",
    "    ldr x25, [x1, #{x25}]",
    "    ldr x26, [x1, #{x26}]",
    "    ldr x27, [x1, #{x27}]",
    "    ldr x28, [x1, #{x28}]",
    "    ldr x29, [x1, #{x29}]",
    "    ldr x2, [x1, #{sp}]",
    "    mov sp, x2",
    "    ldr x30, [x1, #{lr}]",
    "    ldr x2, [x1, #{daif}]",
    "    tst x2, #{daif_i}",
    "    b.ne 1f",
    switch_sti!(),
    "    ret",
    "1:",
    "    ret",
    ".popsection",
    x19 = const CpuContext::X19,
    x20 = const CpuContext::X20,
    x21 = const CpuContext::X21,
    x22 = const CpuContext::X22,
    x23 = const CpuContext::X23,
    x24 = const CpuContext::X24,
    x25 = const CpuContext::X25,
    x26 = const CpuContext::X26,
    x27 = const CpuContext::X27,
    x28 = const CpuContext::X28,
    x29 = const CpuContext::X29,
    daif = const CpuContext::DAIF,
    daif_i = const DAIF_I,
    sp = const CpuContext::SP,
    lr = const CpuContext::LR,
);

unsafe extern "C" {
    fn vibeos_switch_context(old: *mut CpuContext, new: *const CpuContext);
}

/// # Safety
/// As `vibeos::arch::ContextSwitch::switch`.
pub unsafe fn switch_context(old: *mut CpuContext, new: *const CpuContext) {
    // SAFETY: the caller meets `ContextSwitch::switch`; established by
    // `thread_init::switch_now`.
    unsafe { vibeos_switch_context(old, new) };
}
