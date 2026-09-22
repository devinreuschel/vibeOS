#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! STAR / LSTAR / FMASK / SCE, syscall entry asm, FPU, RSP0/CR3 on switch.
//! ROADMAP §9.1. Stub is [`vibeos::syscall::stub`] (ENOSYS only).

use core::arch::global_asm;
use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::desc::{KERNEL_CS, STAR_SYSRET, Tss, USER_CS_RPL, USER_DS_RPL};
use vibeos::per_cpu::PerCpu;
use vibeos::thread::{Fxsave, Tcb};

use crate::arch::gdt;
use crate::per_cpu_init;
use crate::x86::{
    self, CR0_EM, CR0_MP, CR0_TS, CR4_OSFXSR, EFER_SCE, FMASK_SYSCALL, IA32_EFER, IA32_FMASK,
    IA32_GS_BASE, IA32_KERNEL_GS_BASE, IA32_LSTAR, IA32_STAR,
};

static FPU_READY: AtomicBool = AtomicBool::new(false);
static FPU_TEMPLATE: spin_cell::Cell<Fxsave> = spin_cell::Cell::new(Fxsave::empty());

const SCRATCH: usize = offset_of!(PerCpu, syscall_scratch);
const RETVAL: usize = SCRATCH + 8;
const IRET_RIP: usize = SCRATCH + 16;
const IRET_RFLAGS: usize = SCRATCH + 24;
const IRET_RSP: usize = SCRATCH + 32;
const KSP: usize = offset_of!(PerCpu, kernel_rsp0);
const CURRENT: usize = offset_of!(PerCpu, current);
const FPU: usize = offset_of!(Tcb, fpu);
const RF_VM: u64 = (1 << 16) | (1 << 17);

mod spin_cell {
    use core::cell::UnsafeCell;
    pub struct Cell<T>(UnsafeCell<T>);
    unsafe impl<T> Sync for Cell<T> {}
    impl<T> Cell<T> {
        pub const fn new(v: T) -> Self {
            Self(UnsafeCell::new(v))
        }
        pub fn ptr(&self) -> *mut T {
            self.0.get()
        }
    }
}

global_asm!(
    r#"
    .pushsection .text
    .global vibeos_syscall_entry
    .type vibeos_syscall_entry, @function
    vibeos_syscall_entry:
        // Only swapgs besides arch::gs::do_swapgs. User RSP is live.
        swapgs
        mov qword ptr gs:[{user_rsp}], rsp
        mov rsp, qword ptr gs:[{ksp}]

        push r15
        push r14
        push r13
        push r12
        push r11
        push r10
        push r9
        push r8
        push rdi
        push rsi
        push rbp
        push rbx
        push rdx
        push rcx
        push rax
        mov rax, qword ptr gs:[{user_rsp}]
        push rax

        mov rax, qword ptr gs:[{current}]
        test rax, rax
        jz 1f
        fxsave64 [rax + {fpu}]
    1:
        mov rdi, [rsp + 8]
        call vibeos_syscall_stub
        mov qword ptr gs:[{retval}], rax

        mov rax, qword ptr gs:[{current}]
        test rax, rax
        jz 2f
        fxrstor64 [rax + {fpu}]
    2:
        mov rcx, [rsp + 16]
        mov rax, rcx
        sar rax, 47
        cmp rax, 0
        je 3f
        cmp rax, -1
        jne 10f
    3:
        mov r11, [rsp + 88]
        test r11, {rf_vm}
        jnz 10f

        mov rax, qword ptr gs:[{retval}]
        mov rbx, [rsp + 32]
        mov rbp, [rsp + 40]
        mov rsi, [rsp + 48]
        mov rdi, [rsp + 56]
        mov rdx, [rsp + 24]
        mov r8,  [rsp + 64]
        mov r9,  [rsp + 72]
        mov r10, [rsp + 80]
        mov r11, [rsp + 88]
        mov r12, [rsp + 96]
        mov r13, [rsp + 104]
        mov r14, [rsp + 112]
        mov r15, [rsp + 120]
        mov rcx, [rsp + 16]
        mov rsp, [rsp]
        swapgs
        sysretq

    10:
        mov rax, [rsp + 16]
        mov qword ptr gs:[{iret_rip}], rax
        mov rax, [rsp + 88]
        mov qword ptr gs:[{iret_rflags}], rax
        mov rax, [rsp]
        mov qword ptr gs:[{iret_rsp}], rax
        mov rbx, [rsp + 32]
        mov rbp, [rsp + 40]
        mov rsi, [rsp + 48]
        mov rdi, [rsp + 56]
        mov rdx, [rsp + 24]
        mov r8,  [rsp + 64]
        mov r9,  [rsp + 72]
        mov r10, [rsp + 80]
        mov r12, [rsp + 96]
        mov r13, [rsp + 104]
        mov r14, [rsp + 112]
        mov r15, [rsp + 120]
        add rsp, 128
        push {user_ss}
        mov r11, qword ptr gs:[{iret_rsp}]
        push r11
        mov r11, qword ptr gs:[{iret_rflags}]
        push r11
        push {user_cs}
        mov r11, qword ptr gs:[{iret_rip}]
        push r11
        mov rax, qword ptr gs:[{retval}]
        swapgs
        iretq

    .global vibeos_iret_user
    .type vibeos_iret_user, @function
    vibeos_iret_user:
        push {user_ss}
        push rsi
        push rdx
        push {user_cs}
        push rdi
        iretq
    .popsection
    "#,
    user_rsp = const SCRATCH,
    retval = const RETVAL,
    iret_rip = const IRET_RIP,
    iret_rflags = const IRET_RFLAGS,
    iret_rsp = const IRET_RSP,
    ksp = const KSP,
    current = const CURRENT,
    fpu = const FPU,
    rf_vm = const RF_VM,
    user_cs = const USER_CS_RPL as u64,
    user_ss = const USER_DS_RPL as u64,
);

unsafe extern "C" {
    fn vibeos_syscall_entry();
    fn vibeos_iret_user(rip: u64, rsp: u64, rflags: u64) -> !;
}

/// Program SYSCALL MSRs, FPU, and TSS.RSP0 wiring. Per CPU.
///
/// # Safety
/// GDT loaded, `GS_BASE` is this CPU's `PerCpu`.
pub unsafe fn init_cpu() {
    let entry = vibeos_syscall_entry as *const () as u64;
    let star = ((STAR_SYSRET as u64) << 48) | ((KERNEL_CS as u64) << 32);
    unsafe {
        x86::wrmsr(IA32_STAR, star);
        x86::wrmsr(IA32_LSTAR, entry);
        x86::wrmsr(IA32_FMASK, FMASK_SYSCALL);
        let efer = x86::rdmsr(IA32_EFER);
        x86::wrmsr(IA32_EFER, efer | EFER_SCE);
    }
    init_fpu();
}

/// BSP: attach the GDT TSS and the dedicated RSP0 stack.
pub unsafe fn init_bsp() {
    unsafe { init_cpu() };
    let cpu = per_cpu_init::current_mut();
    cpu.tss = gdt::bsp_tss_ptr();
    let top = gdt::bsp_rsp0_top();
    cpu.fallback_rsp0 = top;
    cpu.kernel_rsp0 = top;
    cpu.as_cr3 = crate::paging_init::kernel_cr3();
    seed_current_fpu();
}

/// AP: `tables` is this CPU's GDT/TSS. Call after `install_gs`.
pub unsafe fn init_ap(tss: *mut Tss, rsp0: u64) {
    unsafe { init_cpu() };
    let cpu = per_cpu_init::current_mut();
    cpu.tss = tss;
    cpu.fallback_rsp0 = rsp0;
    cpu.kernel_rsp0 = rsp0;
    cpu.as_cr3 = crate::paging_init::kernel_cr3();
    seed_current_fpu();
}

fn init_fpu() {
    let mut cr0 = x86::read_cr0();
    cr0 &= !(CR0_EM | CR0_TS);
    cr0 |= CR0_MP;
    unsafe { x86::write_cr0(cr0) };
    let cr4 = x86::read_cr4() | CR4_OSFXSR;
    unsafe { x86::write_cr4(cr4) };
    unsafe {
        core::arch::asm!("fninit", options(nomem, nostack));
    }
    if !FPU_READY.load(Ordering::Acquire) {
        let tmpl = FPU_TEMPLATE.ptr();
        unsafe {
            core::arch::asm!(
                "fxsave64 [{p}]",
                p = in(reg) tmpl,
                options(nostack),
            );
        }
        FPU_READY.store(true, Ordering::Release);
    }
}

fn seed_current_fpu() {
    let p = per_cpu_init::current_thread();
    if !p.is_null() {
        unsafe { (*p).fpu = fpu_template() };
    }
}

pub fn fpu_template() -> Fxsave {
    if FPU_READY.load(Ordering::Acquire) {
        unsafe { *FPU_TEMPLATE.ptr() }
    } else {
        Fxsave::empty()
    }
}

/// Update TSS.RSP0 + `kernel_rsp0` for `tcb`. Every context switch.
pub fn set_rsp0_for(tcb: &Tcb) {
    let cpu = per_cpu_init::current_mut();
    let top = match tcb.stack {
        Some(ks) => ks.top(),
        None => cpu.fallback_rsp0,
    };
    cpu.kernel_rsp0 = top;
    if !cpu.tss.is_null() {
        unsafe { (*cpu.tss).set_rsp0(top) };
    }
}

/// Load `tcb`'s CR3 if it differs. Skip when the next thread shares AS.
pub fn switch_cr3_for(tcb: &Tcb) -> bool {
    let want = if tcb.as_cr3 == 0 {
        crate::paging_init::kernel_cr3()
    } else {
        tcb.as_cr3
    };
    let cpu = per_cpu_init::current_mut();
    if cpu.as_cr3 == want || want == 0 {
        return true;
    }
    unsafe { x86::write_cr3(want) };
    cpu.as_cr3 = want;
    false
}

pub fn switch_fpu(old: *mut Tcb, new: *mut Tcb) {
    if !FPU_READY.load(Ordering::Acquire) {
        return;
    }
    unsafe {
        if !old.is_null() {
            let p = core::ptr::addr_of_mut!((*old).fpu);
            core::arch::asm!("fxsave64 [{p}]", p = in(reg) p, options(nostack));
        }
        if !new.is_null() {
            let p = core::ptr::addr_of!((*new).fpu);
            core::arch::asm!("fxrstor64 [{p}]", p = in(reg) p, options(nostack));
        }
    }
}

/// Hardware side of a context switch: FPU, RSP0, CR3. Call before
/// `switch_context`.
pub fn on_switch(old: *mut Tcb, new: *mut Tcb) {
    switch_fpu(old, new);
    if !new.is_null() {
        unsafe {
            set_rsp0_for(&*new);
            switch_cr3_for(&*new);
        }
    }
}

/// `iretq` into ring 3. Does not return; the trampoline `syscall`s then
/// `ud2`s so `catch` can longjmp back.
///
/// # Safety
/// `rip`/`rsp` are mapped executable/writable in the loaded CR3 with
/// user pages. IF in `rflags` should stay clear for Slice A.
pub unsafe fn enter_user(rip: u64, rsp: u64, rflags: u64) -> ! {
    let cpu = per_cpu_init::current();
    let ptr = cpu.self_ptr as u64;
    unsafe {
        x86::wrmsr(IA32_KERNEL_GS_BASE, ptr);
        core::arch::asm!(
            "mov ds, {0:x}",
            "mov es, {0:x}",
            "mov fs, {0:x}",
            "mov gs, {0:x}",
            in(reg) USER_DS_RPL,
            options(nostack, preserves_flags),
        );
        x86::wrmsr(IA32_GS_BASE, 0);
        vibeos_iret_user(rip, rsp, rflags);
    }
}

pub fn star_configured() -> bool {
    let star = x86::rdmsr(IA32_STAR);
    let efer = x86::rdmsr(IA32_EFER);
    let syscall_cs = ((star >> 32) & 0xFFFF) as u16;
    let sysret_cs = ((star >> 48) & 0xFFFF) as u16;
    syscall_cs == KERNEL_CS && sysret_cs == STAR_SYSRET && (efer & EFER_SCE) != 0
}
