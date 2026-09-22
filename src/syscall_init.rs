#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! STAR / LSTAR / FMASK / SCE, syscall entry asm, FPU, RSP0/CR3 on switch.
//! ROADMAP §9.1 / §9.3. Same entry as Slice A; stub is the dispatch table.

use core::arch::global_asm;
use core::mem::offset_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use vibeos::addr_space::AddressSpace;
use vibeos::desc::{KERNEL_CS, STAR_SYSRET, Tss, USER_CS_RPL, USER_DS_RPL};
use vibeos::per_cpu::PerCpu;
use vibeos::syscall::{SyscallFrame, UserRegs};
use vibeos::thread::{Fxsave, RFLAGS_IF, RFLAGS_RESERVED1, Tcb};

use crate::arch::gdt;
use crate::cell::{BootCell, IrqCell};
use crate::per_cpu_init;
use crate::x86::{
    self, CR0_EM, CR0_MP, CR0_TS, CR4_OSFXSR, EFER_SCE, FMASK_SYSCALL, IA32_EFER, IA32_FMASK,
    IA32_FS_BASE, IA32_GS_BASE, IA32_KERNEL_GS_BASE, IA32_LSTAR, IA32_STAR,
};

static FPU_READY: AtomicBool = AtomicBool::new(false);
static FPU_TEMPLATE: BootCell<Fxsave> = BootCell::new();

const SCRATCH: usize = offset_of!(PerCpu, syscall_scratch);
const RETVAL: usize = SCRATCH + 8;
const IRET_RIP: usize = SCRATCH + 16;
const IRET_RFLAGS: usize = SCRATCH + 24;
const IRET_RSP: usize = SCRATCH + 32;
const KSP: usize = offset_of!(PerCpu, kernel_rsp0);
const CURRENT: usize = offset_of!(PerCpu, current);
const FPU: usize = offset_of!(Tcb, fpu);
const RF_VM: u64 = (1 << 16) | (1 << 17);

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
        mov rdi, rsp
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

    .global vibeos_iret_user_full
    .type vibeos_iret_user_full, @function
    vibeos_iret_user_full:
        push {user_ss}
        push qword ptr [rdi + {ur_rsp}]
        push qword ptr [rdi + {ur_rflags}]
        push {user_cs}
        push qword ptr [rdi + {ur_rip}]
        mov rax, [rdi + {ur_rax}]
        mov rbx, [rdi + {ur_rbx}]
        mov rcx, [rdi + {ur_rcx}]
        mov rdx, [rdi + {ur_rdx}]
        mov rsi, [rdi + {ur_rsi}]
        mov rbp, [rdi + {ur_rbp}]
        mov r8,  [rdi + {ur_r8}]
        mov r9,  [rdi + {ur_r9}]
        mov r10, [rdi + {ur_r10}]
        mov r11, [rdi + {ur_r11}]
        mov r12, [rdi + {ur_r12}]
        mov r13, [rdi + {ur_r13}]
        mov r14, [rdi + {ur_r14}]
        mov r15, [rdi + {ur_r15}]
        mov rdi, [rdi + {ur_rdi}]
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
    ur_rax = const offset_of!(UserRegs, rax),
    ur_rbx = const offset_of!(UserRegs, rbx),
    ur_rcx = const offset_of!(UserRegs, rcx),
    ur_rdx = const offset_of!(UserRegs, rdx),
    ur_rsi = const offset_of!(UserRegs, rsi),
    ur_rdi = const offset_of!(UserRegs, rdi),
    ur_rbp = const offset_of!(UserRegs, rbp),
    ur_r8 = const offset_of!(UserRegs, r8),
    ur_r9 = const offset_of!(UserRegs, r9),
    ur_r10 = const offset_of!(UserRegs, r10),
    ur_r11 = const offset_of!(UserRegs, r11),
    ur_r12 = const offset_of!(UserRegs, r12),
    ur_r13 = const offset_of!(UserRegs, r13),
    ur_r14 = const offset_of!(UserRegs, r14),
    ur_r15 = const offset_of!(UserRegs, r15),
    ur_rip = const offset_of!(UserRegs, rip),
    ur_rsp = const offset_of!(UserRegs, rsp),
    ur_rflags = const offset_of!(UserRegs, rflags),
);

unsafe extern "C" {
    fn vibeos_syscall_entry();
    fn vibeos_iret_user(rip: u64, rsp: u64, rflags: u64) -> !;
    fn vibeos_iret_user_full(regs: *const UserRegs) -> !;
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
///
/// # Safety
/// GDT loaded, `GS_BASE` is the BSP `PerCpu`.
pub unsafe fn init_bsp() {
    unsafe { init_cpu() };
    per_cpu_init::with_current(|cpu| {
        cpu.tss = gdt::bsp_tss_ptr();
        let top = gdt::bsp_rsp0_top();
        cpu.fallback_rsp0 = top;
        cpu.kernel_rsp0 = top;
        cpu.as_cr3 = crate::paging_init::kernel_cr3();
    });
    seed_current_fpu();
}

/// AP: `tables` is this CPU's GDT/TSS. Call after `install_gs`.
///
/// # Safety
/// `tss` is this CPU's live TSS; `rsp0` is its kernel stack top.
pub unsafe fn init_ap(tss: *mut Tss, rsp0: u64) {
    unsafe { init_cpu() };
    per_cpu_init::with_current(|cpu| {
        cpu.tss = tss;
        cpu.fallback_rsp0 = rsp0;
        cpu.kernel_rsp0 = rsp0;
        cpu.as_cr3 = crate::paging_init::kernel_cr3();
    });
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
    if FPU_TEMPLATE.try_get().is_none() {
        let mut tmpl = Fxsave::empty();
        unsafe {
            core::arch::asm!(
                "fxsave64 [{p}]",
                p = in(reg) &mut tmpl,
                options(nostack),
            );
        }
        unsafe { FPU_TEMPLATE.set(tmpl) };
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
    FPU_TEMPLATE
        .try_get()
        .copied()
        .unwrap_or_else(Fxsave::empty)
}

/// Update TSS.RSP0 + `kernel_rsp0` for `tcb`. Every context switch.
/// Caller already holds `&mut PerCpu` (IRQ-off).
pub fn set_rsp0_for(cpu: &mut PerCpu, tcb: &Tcb) {
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
/// Caller already holds `&mut PerCpu` (IRQ-off).
pub fn switch_cr3_for(cpu: &mut PerCpu, tcb: &Tcb) -> bool {
    let want = if tcb.as_cr3 == 0 {
        crate::paging_init::kernel_cr3()
    } else {
        tcb.as_cr3
    };
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
/// `switch_context`. Caller already holds `&mut PerCpu` (IRQ-off).
pub fn on_switch(cpu: &mut PerCpu, old: *mut Tcb, new: *mut Tcb) {
    switch_fpu(old, new);
    if !new.is_null() {
        unsafe {
            set_rsp0_for(cpu, &*new);
            switch_cr3_for(cpu, &*new);
        }
    }
}

/// `iretq` into ring 3. Does not return.
///
/// # Safety
/// `rip`/`rsp` are mapped executable/writable in the loaded CR3 with
/// user pages. IF in `rflags` should stay clear unless IRQs in ring 3
/// are intended.
pub unsafe fn enter_user(rip: u64, rsp: u64, rflags: u64, fs_base: u64) -> ! {
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
        x86::wrmsr(IA32_FS_BASE, fs_base);
        vibeos_iret_user(rip, rsp, rflags);
    }
}

/// `iretq` into ring 3 with a full GPR set (fork child / spawned process).
///
/// # Safety
/// `regs.rip`/`regs.rsp` are mapped in the loaded CR3. `regs.rflags`
/// should include the reserved-1 bit.
pub unsafe fn enter_user_full(regs: &UserRegs) -> ! {
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
        x86::wrmsr(IA32_FS_BASE, regs.fs_base);
        vibeos_iret_user_full(regs as *const UserRegs);
    }
}

pub fn star_configured() -> bool {
    let star = x86::rdmsr(IA32_STAR);
    let efer = x86::rdmsr(IA32_EFER);
    let syscall_cs = ((star >> 32) & 0xFFFF) as u16;
    let sysret_cs = ((star >> 48) & 0xFFFF) as u16;
    syscall_cs == KERNEL_CS && sysret_cs == STAR_SYSRET && (efer & EFER_SCE) != 0
}

// --- Slice B: dispatch, early fd1, user exit longjmp ---

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

static TRACE: AtomicBool = AtomicBool::new(false);
static IN_USER: AtomicBool = AtomicBool::new(false);
static EXIT_STATUS: AtomicI32 = AtomicI32::new(0);
static CURRENT_AS: AtomicPtr<AddressSpace> = AtomicPtr::new(ptr::null_mut());
static USER_JMP: IrqCell<JmpBuf> = IrqCell::new(JmpBuf {
    rbx: 0,
    rbp: 0,
    r12: 0,
    r13: 0,
    r14: 0,
    r15: 0,
    rsp: 0,
    rip: 0,
});
static STDOUT_LEN: AtomicUsize = AtomicUsize::new(0);
static STDOUT: IrqCell<[u8; 256]> = IrqCell::new([0; 256]);
static SYSCALLS: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    fn vibeos_user_setjmp(buf: *mut JmpBuf) -> i32;
    fn vibeos_user_longjmp(buf: *mut JmpBuf, val: i32) -> !;
}

global_asm!(
    r#"
    .pushsection .text
    .global vibeos_user_setjmp
    .type vibeos_user_setjmp, @function
    vibeos_user_setjmp:
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

    .global vibeos_user_longjmp
    .type vibeos_user_longjmp, @function
    vibeos_user_longjmp:
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
    .popsection
    "#
);

#[cfg_attr(feature = "kernel_tests", allow(dead_code))] // tracing / procfs; parked
pub fn set_trace(on: bool) {
    TRACE.store(on, Ordering::Release);
}

#[cfg_attr(feature = "kernel_tests", allow(dead_code))] // tracing / procfs; parked
pub fn trace_enabled() -> bool {
    TRACE.load(Ordering::Acquire)
}

#[cfg_attr(feature = "kernel_tests", allow(dead_code))] // tracing / procfs; parked
pub fn syscall_count() -> u64 {
    let t = per_cpu_init::current_thread();
    if !t.is_null() {
        unsafe { (*t).syscall_count }
    } else {
        SYSCALLS.load(Ordering::Relaxed)
    }
}

pub fn reset_stdout() {
    STDOUT_LEN.store(0, Ordering::Release);
}

pub fn stdout_bytes() -> ([u8; 256], usize) {
    STDOUT.with(|buf| {
        let n = STDOUT_LEN.load(Ordering::Acquire).min(256);
        let mut out = [0u8; 256];
        out[..n].copy_from_slice(&buf[..n]);
        (out, n)
    })
}

fn current_as() -> Option<&'static AddressSpace> {
    let p = CURRENT_AS.load(Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        Some(unsafe { &*p })
    }
}

pub fn with_user_as<R>(space: &mut AddressSpace, f: impl FnOnce() -> R) -> R {
    crate::proc_init::bind_probe(space);
    let r = f();
    crate::proc_init::unbind_probe();
    r
}

pub fn peek_user_as() -> Option<&'static AddressSpace> {
    current_as()
}

pub fn set_user_as(space: &AddressSpace) {
    CURRENT_AS.store(
        space as *const AddressSpace as *mut AddressSpace,
        Ordering::Release,
    );
}

pub fn clear_user_as() {
    CURRENT_AS.store(ptr::null_mut(), Ordering::Release);
}

pub fn in_user() -> bool {
    IN_USER.load(Ordering::Acquire)
}

pub fn capture_stdout(bytes: &[u8]) {
    STDOUT.with(|buf| {
        let mut n = STDOUT_LEN.load(Ordering::Relaxed);
        let mut i = 0;
        while i < bytes.len() && n < 256 {
            buf[n] = bytes[i];
            n += 1;
            i += 1;
        }
        STDOUT_LEN.store(n, Ordering::Release);
    });
}

pub fn set_exit_status(st: i32) {
    EXIT_STATUS.store(st, Ordering::Release);
}

pub fn exit_status() -> i32 {
    EXIT_STATUS.load(Ordering::Acquire)
}

pub fn longjmp_user(status: i32) -> ! {
    EXIT_STATUS.store(status, Ordering::Release);
    crate::arch::gs::force_kernel();
    unsafe { vibeos_user_longjmp(USER_JMP.as_ptr(), 1) };
}

fn bump_counter() {
    SYSCALLS.fetch_add(1, Ordering::Relaxed);
    let t = per_cpu_init::current_thread();
    if !t.is_null() {
        unsafe { (*t).syscall_count = (*t).syscall_count.wrapping_add(1) };
    }
}

fn flags_if_on() -> bool {
    let r: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {0}", out(reg) r, options(nostack));
    }
    r & RFLAGS_IF != 0
}

/// Run `rip`/`rsp` in ring 3 until `exit`. Returns the low 8 status bits.
///
/// # Safety
/// `space` is loaded into CR3 and covers `rip`/`rsp`. Lives until return.
pub unsafe fn run_user(space: &mut AddressSpace, rip: u64, rsp: u64, fs_base: u64) -> i32 {
    let if_on = flags_if_on();
    x86::cli();
    reset_stdout();
    EXIT_STATUS.store(0, Ordering::Release);
    CURRENT_AS.store(space as *mut AddressSpace, Ordering::Release);
    let t = per_cpu_init::current_thread();
    if !t.is_null() {
        unsafe { (*t).as_cr3 = space.root().as_u64() };
    }
    crate::addr_space_init::load_cr3(space);
    let rc = unsafe { vibeos_user_setjmp(USER_JMP.as_ptr()) };
    if rc != 0 {
        crate::arch::gs::force_kernel();
        crate::addr_space_init::load_kernel_cr3();
        if !t.is_null() {
            unsafe { (*t).as_cr3 = 0 };
        }
        CURRENT_AS.store(ptr::null_mut(), Ordering::Release);
        IN_USER.store(false, Ordering::Release);
        if if_on {
            x86::sti();
        }
        return EXIT_STATUS.load(Ordering::Acquire) & 0xff;
    }
    IN_USER.store(true, Ordering::Release);
    unsafe { enter_user(rip, rsp, RFLAGS_RESERVED1, fs_base) };
}

#[unsafe(no_mangle)]
pub extern "C" fn vibeos_syscall_stub(frame: *mut SyscallFrame) -> i64 {
    bump_counter();
    crate::proc_init::syscall(frame)
}

pub fn dispatch(nr: u64, args: [u64; 6]) -> i64 {
    crate::proc_init::dispatch(nr, args)
}
