//! The x86_64 context switch: the port's `ContextSwitch` assembly (DESIGN §5.8).
//!
//! The kernel and `tests/hostlib`'s `switch_context` test both compile this
//! file, so it names only `vibeos::` and `core` paths. The includer defines
//! `switch_cli!` and `switch_sti!` before its `mod switch;`: `"cli"` and
//! `"sti"` in the kernel, and `""` on a host, since ring 3 cannot run them.

use vibeos::sched::thread::{CpuContext, RFLAGS_IF};

// `cli` after saving rflags, so the GPR shuffle is not preempted; the
// incoming IF is applied with a delayed `sti` just before the `jmp`.
core::arch::global_asm!(
    ".pushsection .text",
    ".global vibeos_switch_context",
    ".type vibeos_switch_context, @function",
    "vibeos_switch_context:",
    "    mov rax, [rsp]",
    "    mov [rdi + {rip}], rax",
    "    lea rax, [rsp + 8]",
    "    mov [rdi + {rsp}], rax",
    "    mov [rdi + {rbx}], rbx",
    "    mov [rdi + {rbp}], rbp",
    "    mov [rdi + {r12}], r12",
    "    mov [rdi + {r13}], r13",
    "    mov [rdi + {r14}], r14",
    "    mov [rdi + {r15}], r15",
    "    pushfq",
    "    pop rax",
    "    mov [rdi + {rflags}], rax",
    switch_cli!(),
    "    mov rbx, [rsi + {rbx}]",
    "    mov rbp, [rsi + {rbp}]",
    "    mov r12, [rsi + {r12}]",
    "    mov r13, [rsi + {r13}]",
    "    mov r14, [rsi + {r14}]",
    "    mov r15, [rsi + {r15}]",
    "    mov rsp, [rsi + {rsp}]",
    "    mov rax, [rsi + {rflags}]",
    "    test rax, {rflags_if}",
    "    jz 2f",
    "    and rax, {rflags_no_if}",
    "    push rax",
    "    popfq",
    switch_sti!(),
    "    jmp qword ptr [rsi + {rip}]",
    "2:",
    "    push rax",
    "    popfq",
    "    jmp qword ptr [rsi + {rip}]",
    ".popsection",
    rbx = const CpuContext::RBX,
    rbp = const CpuContext::RBP,
    r12 = const CpuContext::R12,
    r13 = const CpuContext::R13,
    r14 = const CpuContext::R14,
    r15 = const CpuContext::R15,
    rflags = const CpuContext::RFLAGS,
    rflags_if = const RFLAGS_IF,
    rflags_no_if = const !RFLAGS_IF,
    rsp = const CpuContext::RSP,
    rip = const CpuContext::RIP,
);

unsafe extern "C" {
    fn vibeos_switch_context(old: *mut CpuContext, new: *const CpuContext);
}

/// Save callee-saved GPRs, rflags, rsp, return address; restore `new`.
///
/// Kernel builds `cli` after the save so a timer cannot observe mixed
/// GPRs. Incoming IF is applied with delayed `sti` immediately before
/// `jmp`, not `popfq` with IF set: a tick in that window preempts a
/// first-run thread, `schedule_preempt` overwrites the synthetic
/// trampoline frame, and `iret` then `jmp`s into `schedule_inner` on
/// `stack_top-8`. Host tests skip `cli` (ring 3). No FPU/SSE.
///
/// # Safety
/// `old` is valid for writes of a `CpuContext` and `new` for reads of one,
/// and neither is written by another CPU during the call. `new` was saved
/// by `switch_context` or seeded by `thread::prepare_thread`, so `new.rsp`
/// points into a live stack whose frame `new.rip` expects. The caller is
/// not using the red zone below either `rsp`.
pub unsafe fn switch_context(old: *mut CpuContext, new: *const CpuContext) {
    // SAFETY: the caller meets this fn's `# Safety` contract, which is the
    // asm's whole requirement; established by `switch::switch_context`'s
    // callers, `thread_init::switch_now` in the kernel.
    unsafe { vibeos_switch_context(old, new) };
}
