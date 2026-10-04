//! In-guest tests for arch (kernel_tests only). Rows: [`TESTS`].

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::addr_space::UserPerms;
use vibeos::apic::TimerMode;
use vibeos::desc::{IstSlot, KERNEL_CS, TSS_SEL, star_value, sysret_selectors};
use vibeos::kva::PAGE_SIZE;
use vibeos::paging::PAGE_SIZE_4K;
use vibeos::proc::{SIGBUS, SIGFPE, SIGILL, SIGKILL, SIGSEGV, SIGTRAP, wait_signaled};
use vibeos::syscall::SYS_KILL;
use vibeos::vectors;

use crate::addr_space_init;
use crate::apic_init;
use crate::arch;
use crate::arch::idt::{TrapFrame, testing};
use crate::ipi_init;
use crate::irq_init;
use crate::ktest::user::{self, DEFAULT, Image, user_code};
use crate::ktest::{Outcome, Test, spawn_thread_on, spin_until_ns, test};
use crate::kva_init;
use crate::machine_init;
use crate::per_cpu_init;
use crate::proc_init;
use crate::thread_init;
use crate::time_init;
use crate::x86;

mod control;
mod idt;
mod ipi;
mod msr;
mod seam;
mod timer;
mod uaccess;

pub(crate) use idt::test_idt_set_handler_refuses_fixed;
pub(crate) use ipi::test_ipi_icr_writes_if_off;
pub(crate) use seam::test_arch_seam_core;
pub(crate) use uaccess::*;

/// Whether the I/O APIC entry for `gsi` is masked; `None` when no I/O
/// APIC serves it.
pub(crate) fn gsi_masked(gsi: u32) -> Option<bool> {
    apic_init::with_state(|st| {
        let (io, pin) = apic_init::find_ioapic(st, gsi)?;
        let (lo, _) = vibeos::apic::ioapic_redir_regs(pin);
        // SAFETY: invariant I49, established at
        // `arch::x86_64::apic_init::enum_ioapics`: `io.va` is set only
        // there, and `with_state` holds `STATE`.
        Some(vibeos::apic::redir_is_masked(unsafe {
            apic_init::io_read(io.va, lo)
        }))
    })
}

/// Whether `apic_init::init` finished.
fn apic_is_ready() -> bool {
    apic_init::with_state(|st| st.ready)
}

/// `[base, top)` of one of the BSP's IST stacks.
fn ist_span(slot: IstSlot) -> (u64, u64) {
    let s = &arch::gdt::BSP.get().ist[slot.index()];
    (s.base().as_u64(), s.top().as_u64())
}

/// Current code selector.
fn read_cs() -> u16 {
    let val: u16;
    // SAFETY: `mov r, cs` only copies the CS selector into a register;
    // established here.
    unsafe {
        core::arch::asm!("mov {0:x}, cs", out(reg) val, options(nomem, nostack, preserves_flags))
    };
    val
}

/// Task register, which the GDT test compares with the TSS selector.
fn read_tr() -> u16 {
    let val: u16;
    // SAFETY: `str` only copies the task register into a register;
    // established here.
    unsafe {
        core::arch::asm!("str {0:x}", out(reg) val, options(nomem, nostack, preserves_flags))
    };
    val
}

unsafe extern "C" {
    fn vibeos_write_u8_1(addr: u64);
    fn vibeos_fault_on_bad_stack(rsp: u64) -> !;
}

// Known-length store (`C6 07 01`, 3 bytes) for the skip-RIP catcher.
// `ud2` on an unmapped RSP forces #UD delivery to fail into #DF on IST1.
global_asm!(
    r#"
    .pushsection .text
    .global vibeos_write_u8_1
    vibeos_write_u8_1:
        mov byte ptr [rdi], 1
        ret
    .global vibeos_fault_on_bad_stack
    vibeos_fault_on_bad_stack:
        mov rsp, rdi
        ud2
    .popsection
    "#
);

const WRITE_U8_1_LEN: u8 = 3;

pub(crate) fn test_gdt_selectors() -> Outcome {
    if read_cs() != KERNEL_CS {
        return Outcome::Fail("cs not kernel code");
    }
    if TSS_SEL != 0x38 || read_tr() != TSS_SEL {
        return crate::fail_fmt!("tr {:#x}, TSS_SEL {TSS_SEL:#x}, want 0x38", read_tr());
    }
    Outcome::Ok
}

pub(crate) fn test_star_sysret_layout() -> Outcome {
    if star_value() != 0x0023_0008_0000_0000 || sysret_selectors(star_value()) != (0x33, 0x2b) {
        return crate::fail_fmt!("star_value {:#x}: bad SYSRET", star_value());
    }
    if !crate::proc::ktest::star_configured() {
        return Outcome::Fail("IA32_STAR != star_value() or EFER.SCE clear");
    }
    Outcome::Ok
}

/// A kernel `int3` returns, and the `#BP` body ran for it exactly once
/// (ROADMAP §10.2, F142).
pub(crate) fn test_int3_roundtrip() -> Outcome {
    let before = testing::bp_hits();
    // SAFETY: `int3` raises `#BP`, whose CPL-0 body returns past it, and
    // touches nothing else; established by `arch::x86_64::idt::init`, which
    // gives `#BP` its gate.
    unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
    let ran = testing::bp_hits().wrapping_sub(before);
    if ran != 1 {
        return crate::fail_fmt!("#BP body ran {ran} times, want 1");
    }
    Outcome::Ok
}

// `int3` on the CPU that armed the catch, and on another one. Both
// `#[inline(never)]`, so each `int3` lies in its own function.
#[inline(never)]
fn int3_here() {
    // SAFETY: `int3` raises `#BP`, whose CPL-0 body returns past it, and
    // touches nothing else; established by `arch::x86_64::idt::init`, which
    // gives `#BP` its gate.
    unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
}

#[inline(never)]
fn int3_other() {
    // SAFETY: `int3` raises `#BP`, whose CPL-0 body returns past it, and
    // touches nothing else; established by `arch::x86_64::idt::init`, which
    // gives `#BP` its gate.
    unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
}

/// Run `int3_other` once `INT3_GO` is set, then set `INT3_DONE`. A kernel
/// thread pinned on the other CPU, not call-function work, since the `#BP`
/// body logs and call-function work takes no lock (DESIGN §2.2).
static INT3_GO: AtomicBool = AtomicBool::new(false);
static INT3_DONE: AtomicBool = AtomicBool::new(false);

fn int3_thread() {
    // Acquire: pairs with the Release store of `INT3_GO` in the test.
    if spin_until_ns(|| INT3_GO.load(Ordering::Acquire), 2_000_000_000) {
        int3_other();
    }
    INT3_DONE.store(true, Ordering::Release);
}

/// Whether `rip`, a `#BP` return address, lies in the function at `f`:
/// past its start, near it, and with no start of `g` in between.
fn rip_in(rip: u64, f: u64, g: u64) -> bool {
    rip > f && rip - f < 64 && !(g > f && g < rip)
}

/// A `#BP` on another CPU during a catch window takes its normal path (log
/// and continue), and the window catches this CPU's `#BP` (F146).
pub(crate) fn test_catch_ignores_other_cpu() -> Outcome {
    let me = x86::cpu_index().unwrap_or(0);
    let mask = per_cpu_init::online_mask() & !(1u64 << me);
    if mask == 0 {
        return Outcome::Skip("one CPU");
    }
    let other = mask.trailing_zeros();
    let here = int3_here as *const () as u64;
    let there = int3_other as *const () as u64;
    // insn_len 0: `#BP` is a trap, its saved RIP is already past `int3`.
    INT3_GO.store(false, Ordering::Relaxed);
    INT3_DONE.store(false, Ordering::Relaxed);
    let _t = spawn_thread_on("int3-other", int3_thread, other);
    let mut ran = false;
    let caught = arch::catch::catch_skip(vectors::BP, 0, || {
        INT3_GO.store(true, Ordering::Release);
        ran = spin_until_ns(|| INT3_DONE.load(Ordering::Acquire), 2_000_000_000);
        int3_here();
    });
    if !ran {
        return Outcome::Fail("the other cpu's int3 thread did not run");
    }
    let Some(c) = caught else {
        return Outcome::Fail("no #BP caught");
    };
    if rip_in(c.frame.rip, here, there) {
        return Outcome::Ok;
    }
    if rip_in(c.frame.rip, there, here) {
        return crate::fail_fmt!(
            "caught cpu {other}'s #BP at {:#x}, not this cpu's {me}",
            c.frame.rip
        );
    }
    crate::fail_fmt!("caught rip {:#x} in neither int3 fn", c.frame.rip)
}

// `ud2`, then exit(0) if the kernel stepped past it.
user_code!(
    UD2_EXIT0,
    "
    ud2
    xor edi, edi
    mov eax, 60
    syscall
    "
);

/// A ring-3 `#UD` during a catch window for `#UD` takes its ring-3 path
/// (SIGILL), and the window catches nothing (F146).
pub(crate) fn test_catch_ignores_user_frame() -> Outcome {
    let mut st = None;
    let caught = arch::catch::catch_skip(vectors::UD, 2, || {
        st = Some(user::run(&Image::Code(UD2_EXIT0, DEFAULT), &["ud2_exit0"]));
    });
    let st = match st {
        Some(Ok(st)) => st,
        Some(Err(e)) => return crate::fail_fmt!("spawn: {}", e.as_str()),
        None => return Outcome::Fail("catch body did not run"),
    };
    if st != wait_signaled(SIGILL) {
        return crate::fail_fmt!("status {st:#x}, want SIGILL");
    }
    if let Some(c) = caught {
        return crate::fail_fmt!("caught a CPL-{} #UD at {:#x}", c.frame.cs & 3, c.frame.rip);
    }
    Outcome::Ok
}

pub(crate) fn test_scoped_pf() -> Outcome {
    let Some(va) = crate::mm::ktest::alloc_va(PAGE_SIZE) else {
        return Outcome::Fail("kva alloc");
    };
    // SAFETY: `va` is a reserved, unmapped kernel page, so the one-byte
    // store faults and `catch_skip` steps past its `WRITE_U8_1_LEN` bytes
    // before it writes anything; established here.
    let caught = arch::catch::catch_skip(vectors::PF, WRITE_U8_1_LEN, || unsafe {
        vibeos_write_u8_1(va.as_u64());
    });
    crate::mm::ktest::free_va(va, PAGE_SIZE);
    let Some(c) = caught else {
        return Outcome::Fail("store did not fault");
    };
    if c.vector != vectors::PF {
        return Outcome::Fail("wrong vector");
    }
    if (c.cr2 & !0xFFF) != (va.as_u64() & !0xFFF) {
        return Outcome::Fail("cr2 not the unmapped page");
    }
    Outcome::Ok
}

pub(crate) fn test_gp_catch() -> Outcome {
    let g = x86::InterruptGuard::enter();
    // SAFETY: selector 0x0B is not a data descriptor, so the load raises
    // `#GP` before DS changes, and `catch` longjmps out with IRQs off (`g`);
    // established here.
    let caught = arch::catch::catch(vectors::GP, || unsafe {
        core::arch::asm!(
            "mov ds, {0:x}",
            in(reg) 0x0Bu16,
            options(nostack, preserves_flags)
        );
    });
    drop(g);
    match caught {
        Some(c)
            if c.vector == vectors::GP && c.frame.cs == KERNEL_CS as u64 && c.frame.rip != 0 =>
        {
            Outcome::Ok
        }
        Some(_) => Outcome::Fail("wrong vector or frame"),
        None => Outcome::Fail("no gp"),
    }
}

pub(crate) fn test_df_on_ist() -> Outcome {
    let Ok(stack) = kva_init::alloc_guarded_stack(1) else {
        return Outcome::Fail("guarded stack");
    };
    let poison = stack.guard().as_u64() + 0x800;
    let g = x86::InterruptGuard::enter();
    // SAFETY: `poison` lies in `stack`'s unmapped guard page, so the first
    // push faults, the fault's own push faults again, and `#DF` runs on its
    // IST stack, where `catch` longjmps back with IRQs off (`g`);
    // established here.
    let caught = arch::catch::catch(vectors::DF, || unsafe {
        vibeos_fault_on_bad_stack(poison);
    });
    drop(g);
    kva_init::free_stack(stack);
    let Some(c) = caught else {
        return Outcome::Fail("did not reach df handler");
    };
    let (lo, hi) = ist_span(IstSlot::DoubleFault);
    if c.handler_rsp >= lo && c.handler_rsp < hi {
        Outcome::Ok
    } else {
        crate::marker!(
            "vibeOS: ktest:   rsp={:#x} lo={:#x} hi={:#x}",
            c.handler_rsp,
            lo,
            hi
        );
        Outcome::Fail("handler rsp not on ist1")
    }
}

pub(crate) fn test_lapic_timer_mode() -> Outcome {
    if !apic_is_ready() {
        return Outcome::Fail("lapic not ready");
    }
    if !apic_init::owns_tick() && apic_init::timer_mode() != TimerMode::Pit {
        return Outcome::Fail("lapic mode without owning tick");
    }
    let mode = apic_init::timer_mode();
    let cpuid = apic_init::cpuid_tsc_deadline();
    match (cpuid, mode) {
        (true, TimerMode::TscDeadline) => Outcome::Ok,
        (true, TimerMode::Periodic | TimerMode::Pit) => {
            Outcome::Fail("silent downgrade from tsc-deadline")
        }
        (false, TimerMode::Periodic) => Outcome::Ok,
        (false, TimerMode::Pit) => {
            if machine_init::info().is_some_and(|d| d.hpet_info().is_some()) {
                Outcome::Fail("pit despite hpet")
            } else {
                Outcome::Ok
            }
        }
        (false, TimerMode::TscDeadline) => Outcome::Fail("tsc-deadline without cpuid"),
    }
}

pub(crate) fn test_ioapic_pit_gsi_masked() -> Outcome {
    match apic_init::timer_mode() {
        TimerMode::Pit => Outcome::Skip("pit owns tick"),
        TimerMode::TscDeadline | TimerMode::Periodic => {
            let Some(desc) = machine_init::info() else {
                return Outcome::Fail("no machine desc");
            };
            let gsi = vibeos::apic::gsi_for_isa_irq(0, desc.irq_overrides());
            match gsi_masked(gsi) {
                Some(true) => Outcome::Ok,
                Some(false) => Outcome::Fail("pit gsi unmasked"),
                None => Outcome::Fail("pit gsi not on ioapic"),
            }
        }
    }
}

pub(crate) fn test_irq_guard_nest() -> Outcome {
    if !x86::interrupts_enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    let nest0 = per_cpu_init::irq_nest();
    {
        let g1 = x86::InterruptGuard::enter();
        if x86::interrupts_enabled() {
            return Outcome::Fail("g1 left IF on");
        }
        if per_cpu_init::irq_nest() != nest0 + 1 {
            return Outcome::Fail("g1 nest");
        }
        {
            let g2 = x86::InterruptGuard::enter();
            if x86::interrupts_enabled() {
                return Outcome::Fail("g2 left IF on");
            }
            if per_cpu_init::irq_nest() != nest0 + 2 {
                return Outcome::Fail("g2 nest");
            }
            core::mem::drop(g2);
        }
        if x86::interrupts_enabled() {
            return Outcome::Fail("after g2 IF on");
        }
        if per_cpu_init::irq_nest() != nest0 + 1 {
            return Outcome::Fail("after g2 nest");
        }
        core::mem::drop(g1);
    }
    if !x86::interrupts_enabled() {
        return Outcome::Fail("outer drop did not restore IF");
    }
    if per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("nest not restored");
    }
    Outcome::Ok
}

struct CrSnap {
    cr0: AtomicU64,
    cr4: AtomicU64,
}

fn read_cr_remote(arg: *mut ()) {
    // SAFETY: `arg` is the `CrSnap` that `test_cpu_hardening` passes to a
    // waiting `ipi_init::call_cpu`, live until every target acks; established
    // at `arch::ktest::test_cpu_hardening`.
    let s = unsafe { &*(arg as *const CrSnap) };
    s.cr0.store(x86::read_cr0(), Ordering::SeqCst);
    s.cr4.store(x86::read_cr4(), Ordering::SeqCst);
}

pub(crate) fn test_cpu_hardening() -> Outcome {
    let f = arch::cpu::cpuid_features();
    if !f.smep && !f.smap && !f.umip {
        return Outcome::Skip("no smep/smap/umip");
    }
    x86::clac();
    crate::arch::x86_64::uaccess::with_window(|| ());
    x86::clac();

    let mask = per_cpu_init::online_mask();
    let mut cpu = 0u32;
    while cpu < 64 {
        if mask & (1u64 << cpu) == 0 {
            cpu += 1;
            continue;
        }
        let snap = CrSnap {
            cr0: AtomicU64::new(0),
            cr4: AtomicU64::new(0),
        };
        {
            // IF=0: the id and the local reads name one CPU (DESIGN §2.9
            // rule 5); `call_cpu` waits for its ack with IF=0 anyway.
            let _g = x86::InterruptGuard::enter();
            if cpu == per_cpu_init::current().cpu_id {
                snap.cr0.store(x86::read_cr0(), Ordering::SeqCst);
                snap.cr4.store(x86::read_cr4(), Ordering::SeqCst);
            } else {
                // `read_cr_remote` stores only to `snap`'s atomics, through `&`.
                // PROVENANCE: nothing writes through the pointer.
                ipi_init::call_cpu(cpu, read_cr_remote, &snap as *const _ as *mut (), true);
            }
        }
        let cr0 = snap.cr0.load(Ordering::SeqCst);
        let cr4 = snap.cr4.load(Ordering::SeqCst);
        if cr0 & x86::CR0_WP == 0 {
            return Outcome::Fail("wp");
        }
        if f.smep != (cr4 & x86::CR4_SMEP != 0) {
            return Outcome::Fail("smep");
        }
        if f.smap != (cr4 & x86::CR4_SMAP != 0) {
            return Outcome::Fail("smap");
        }
        if f.umip != (cr4 & x86::CR4_UMIP != 0) {
            return Outcome::Fail("umip");
        }
        cpu += 1;
    }
    Outcome::Ok
}

const RFLAGS_AC: u64 = 1 << 18;

/// Page-fault error code bit 0: the page was present (a protection fault).
const PF_PRESENT: u64 = 1;

/// The user page both AC tests read: `user::DEFAULT`'s load address.
const USER_VA: u64 = 0x4000_0000;

/// What an AC hook saw: the frame's AC, then its stray read's fault.
struct AcProbe {
    ran: AtomicBool,
    frame_ac: AtomicBool,
    faulted: AtomicBool,
    cr2: AtomicU64,
    error: AtomicU64,
}

impl AcProbe {
    const fn new() -> Self {
        Self {
            ran: AtomicBool::new(false),
            frame_ac: AtomicBool::new(false),
            faulted: AtomicBool::new(false),
            cr2: AtomicU64::new(0),
            error: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.frame_ac.store(false, Ordering::Relaxed);
        self.faulted.store(false, Ordering::Relaxed);
        self.cr2.store(0, Ordering::Relaxed);
        self.error.store(0, Ordering::Relaxed);
        self.ran.store(false, Ordering::Release);
    }

    /// Record the frame's AC, then read `USER_VA` from the body's context,
    /// where the stub has cleared AC, so SMAP must fault. Publishes `ran`
    /// last.
    fn probe(&self, frame: &TrapFrame) {
        self.frame_ac
            .store(frame.iret.rflags & RFLAGS_AC != 0, Ordering::Relaxed);
        // SAFETY: invariant: `USER_VA` is a present user page in the loaded
        // address space, so the read is a SMAP `#PF` that `catch_fault`
        // catches, or a plain read if AC leaked; established by the test
        // that installs the hook (`arch::ktest::test_ac_clear_on_exception`
        // loads its space; in `test_ac_clear_user_popf` it is the child's
        // code page).
        let hit = crate::ktest::catch_fault(|| unsafe {
            core::ptr::read_volatile(USER_VA as *const u8);
        });
        if let Some(f) = hit {
            self.faulted.store(true, Ordering::Relaxed);
            self.cr2.store(f.cr2, Ordering::Relaxed);
            self.error.store(f.error, Ordering::Relaxed);
        }
        self.ran.store(true, Ordering::Release);
    }

    /// `None` when the hook saw AC set in the frame and its read took a
    /// SMAP protection fault at `USER_VA`.
    fn verdict(&self) -> Option<Outcome> {
        if !self.ran.load(Ordering::Acquire) {
            return Some(Outcome::Fail("hook did not run"));
        }
        if !self.frame_ac.load(Ordering::Relaxed) {
            return Some(Outcome::Fail("frame AC clear"));
        }
        if !self.faulted.load(Ordering::Relaxed) {
            return Some(Outcome::Fail("stray user read did not fault: AC still set"));
        }
        let (cr2, error) = (
            self.cr2.load(Ordering::Relaxed),
            self.error.load(Ordering::Relaxed),
        );
        if cr2 != USER_VA || error & PF_PRESENT == 0 {
            return Some(crate::fail_fmt!(
                "fault cr2={cr2:#x} err={error:#x}, want {USER_VA:#x} present"
            ));
        }
        None
    }
}

/// Free frames once no dead thread's stack an earlier test left is still
/// on its way back, since it would land inside this test's count.
fn settled_frames() -> usize {
    // A shortfall shows as a frame mismatch in the caller.
    let _ = crate::ktest::settle_threads();
    crate::ktest::free_frames()
}

static BP_PROBE: AcProbe = AcProbe::new();

fn bp_hook(frame: &mut TrapFrame) -> bool {
    BP_PROBE.probe(frame);
    false
}

pub(crate) fn test_ac_clear_on_exception() -> Outcome {
    if !x86::smap_live() {
        return Outcome::Skip("no SMAP");
    }
    let before = settled_frames();
    let Ok(space) = addr_space_init::create() else {
        return Outcome::Fail("create");
    };
    // SAFETY: invariant: `space` is a fresh address space that no CPU has
    // loaded, and the range is page-aligned user space; established here.
    if unsafe { addr_space_init::map_anon(&space, USER_VA, PAGE_SIZE_4K, UserPerms::RW) }.is_err() {
        return Outcome::Fail("map_anon");
    }
    BP_PROBE.reset();
    let ac_after = {
        let _g = x86::InterruptGuard::enter();
        crate::proc::ktest::load_cr3(&space);
        x86::invlpg(USER_VA);
        testing::set_hook(vectors::BP, Some(bp_hook));
        let ac = crate::arch::x86_64::uaccess::with_window(|| {
            // SAFETY: invariant: `int3` at CPL 0 reaches `breakpoint`, which
            // logs and returns; established by `arch::idt::init`.
            unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
            x86::rflags() & RFLAGS_AC != 0
        });
        testing::set_hook(vectors::BP, None);
        addr_space_init::load_kernel_cr3();
        ac
    };
    // The last `users` put, with the kernel CR3 back.
    drop(space);
    if let Some(fail) = BP_PROBE.verdict() {
        return fail;
    }
    if !ac_after {
        return Outcome::Fail("iretq did not restore AC");
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, crate::ktest::free_frames());
    }
    Outcome::Ok
}

// Set RFLAGS.AC with popf, then raise #UD; exit(1) if ud2 returns.
user_code!(
    POPF_AC_UD2,
    "
    pushfq
    or dword ptr [rsp], 0x40000
    popfq
    ud2
    mov edi, 1
    mov eax, 60
    syscall
    "
);

static UD_PROBE: AcProbe = AcProbe::new();

fn ud_hook(frame: &mut TrapFrame) -> bool {
    if frame.user_mode() {
        UD_PROBE.probe(frame);
    }
    false
}

pub(crate) fn test_ac_clear_user_popf() -> Outcome {
    if !x86::smap_live() {
        return Outcome::Skip("no SMAP");
    }
    let before = settled_frames();
    UD_PROBE.reset();
    testing::set_hook(vectors::UD, Some(ud_hook));
    let st = user::run(&Image::Code(POPF_AC_UD2, DEFAULT), &["popf_ac"]);
    testing::set_hook(vectors::UD, None);
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if let Some(fail) = UD_PROBE.verdict() {
        return fail;
    }
    if st != wait_signaled(SIGILL) {
        return crate::fail_fmt!("status {st:#x}, want SIGILL");
    }
    if !user::frames_settle(before) {
        return crate::fail_fmt!("frame leak: {} -> {}", before, crate::ktest::free_frames());
    }
    Outcome::Ok
}

// Hardware execute breakpoints on the three syscall instructions that run
// at CPL 0 with the user GS base loaded (DESIGN §5.10 rule 3): DR0 on the
// entry `swapgs`, DR1 on the `sysretq` and DR2 on the `iretq` after the
// exits' `swapgs`. Test-only helpers; nothing else in the kernel uses the
// debug registers.

unsafe extern "C" {
    static vibeos_syscall_entry: [u8; 3];
    static vibeos_syscall_exit_swapgs: [u8; 5];
    static vibeos_syscall_iret_swapgs: [u8; 5];
}

const SWAPGS: [u8; 3] = [0x0F, 0x01, 0xF8];

const SYSRETQ: [u8; 3] = [0x48, 0x0F, 0x07];

const IRETQ: [u8; 2] = [0x48, 0xCF];

const RFLAGS_RF: u64 = 1 << 16;

/// DR7 L0, L1 and L2; R/W and LEN zero: 1-byte execute breakpoints.
const DR7_ARM: u64 = 0b01_0101;

static DB_ADDR: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

static DB_HITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

static DB_BAD: AtomicU64 = AtomicU64::new(0);

static DB_ENTRIES: AtomicU32 = AtomicU32::new(0);

/// Write DR0 to DR2 from `DB_ADDR`, then DR7.
fn arm_db(_: *mut ()) {
    let a = |i: usize| DB_ADDR[i].load(Ordering::Acquire);
    // SAFETY: invariant: the addresses are kernel text and DR7 enables
    // only execute breakpoints on them, which `db_hook` resumes past;
    // established by `arch::ktest::test_ist_gs_sign`, which clears them on
    // every path.
    unsafe {
        core::arch::asm!(
            "mov dr0, {0}",
            "mov dr1, {1}",
            "mov dr2, {2}",
            "mov dr7, {3}",
            in(reg) a(0),
            in(reg) a(1),
            in(reg) a(2),
            in(reg) DR7_ARM,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Clear DR7, then DR0 to DR2.
fn disarm_db(_: *mut ()) {
    // SAFETY: invariant: zero in DR7 disables every breakpoint, and the
    // kernel keeps no other state in DR0 to DR2; established here.
    unsafe {
        core::arch::asm!(
            "mov dr7, {0}",
            "mov dr0, {0}",
            "mov dr1, {0}",
            "mov dr2, {0}",
            in(reg) 0u64,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// On every CPU, this one included.
fn on_all_cpus(f: fn(*mut ())) {
    ipi_init::call_mask(per_cpu_init::online_mask(), f, core::ptr::null_mut(), true);
    let _g = x86::InterruptGuard::enter();
    f(core::ptr::null_mut());
}

/// This CPU's `PerCpu`, found through GS, is the one its APIC ID names.
fn percpu_ok() -> bool {
    let Some(cpu) = per_cpu_init::try_current() else {
        return false;
    };
    if !core::ptr::eq(cpu.self_ptr.cast_const(), cpu) {
        return false;
    }
    let apic = x86::cpuid(1, 0).1 >> 24;
    crate::ktest::cpu_remote(cpu.cpu_id).is_some_and(|c| c.apic_id.load(Ordering::Relaxed) == apic)
}

fn db_hook(frame: &mut TrapFrame) -> bool {
    let Some(slot) = (0..3).find(|&i| frame.dr6 & (1 << i) != 0) else {
        return false;
    };
    if !percpu_ok() {
        DB_BAD.fetch_add(1, Ordering::Relaxed);
    }
    if slot == 0 && DB_ENTRIES.fetch_add(1, Ordering::Relaxed) % 2 == 1 {
        // The user RFLAGS that `syscall` left in r11: RF sends this call's
        // exit down the `iretq` path.
        frame.user_mut().r11 |= RFLAGS_RF;
    }
    // Resume past the breakpoint instead of taking it again.
    frame.iret.rflags |= RFLAGS_RF;
    DB_HITS[slot].fetch_add(1, Ordering::Release);
    true
}

// getpid forever, with a short spin between calls. Ends by SIGKILL, which
// acts at its next syscall entry.
user_code!(
    GETPID_LOOP,
    "
2:
    mov ecx, 2000
1:
    pause
    dec ecx
    jnz 1b
    mov eax, 39
    syscall
    jmp 2b
    "
);

/// CPL-3 entries on every vector so far.
fn cpl3_total() -> u64 {
    (0..=255u8).fold(0, |n, v| n.wrapping_add(testing::cpl3_hits(v)))
}

/// Yield until `pred` holds, for at most `ms`.
fn wait_until(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::yield_now();
    }
    true
}

pub(crate) fn test_ist_gs_sign() -> Outcome {
    let entry = (&raw const vibeos_syscall_entry).cast::<u8>();
    let sysret = (&raw const vibeos_syscall_exit_swapgs)
        .cast::<u8>()
        .wrapping_add(3);
    let iret = (&raw const vibeos_syscall_iret_swapgs)
        .cast::<u8>()
        .wrapping_add(3);
    // SAFETY: invariant: each symbol is a label in `syscall_init`'s entry
    // asm with at least that many bytes of kernel text after it, mapped
    // for the kernel's life; established by the `global_asm!` in
    // `proc::syscall_init`, whose `syscall_init::first_return` jumps into it.
    let bytes = unsafe {
        (
            core::slice::from_raw_parts(entry, 3),
            core::slice::from_raw_parts(sysret.wrapping_sub(3), 6),
            core::slice::from_raw_parts(iret.wrapping_sub(3), 5),
        )
    };
    if bytes.0 != SWAPGS
        || bytes.1[..3] != SWAPGS
        || bytes.1[3..] != SYSRETQ
        || bytes.2[..3] != SWAPGS
        || bytes.2[3..] != IRETQ
    {
        return Outcome::Fail("syscall labels moved off swapgs; sysretq / swapgs; iretq");
    }
    for (slot, a) in [entry, sysret, iret].into_iter().enumerate() {
        DB_ADDR[slot].store(a as u64, Ordering::Release);
        DB_HITS[slot].store(0, Ordering::Relaxed);
    }
    DB_BAD.store(0, Ordering::Relaxed);
    DB_ENTRIES.store(0, Ordering::Relaxed);
    let cpl3_before = cpl3_total();
    let pid = match user::spawn(&Image::Code(GETPID_LOOP, DEFAULT), &["getpid_loop"]) {
        Ok(pid) => pid,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    // Arm once the child has taken an interrupt in ring 3, so it is past
    // its first return (`syscall_init::first_return`), whose selector and
    // MSR block this test does not cover.
    let in_ring3 = wait_until(|| cpl3_total() != cpl3_before, 5_000);
    if in_ring3 {
        testing::set_hook(vectors::DB, Some(db_hook));
        on_all_cpus(arm_db);
        // A shortfall shows in the hit counts below.
        let _ = wait_until(
            || {
                let h = |i: usize| DB_HITS[i].load(Ordering::Acquire);
                h(0) >= 4 && h(1) >= 1 && h(2) >= 1
            },
            5_000,
        );
        on_all_cpus(disarm_db);
        testing::set_hook(vectors::DB, None);
    }
    let _ = proc_init::dispatch(SYS_KILL, [pid as u64, SIGKILL as u64, 0, 0, 0, 0]);
    let st = user::wait(pid);
    if !in_ring3 {
        return Outcome::Fail("child took no interrupt in ring 3");
    }
    if st != wait_signaled(SIGKILL) {
        return crate::fail_fmt!("status {st:#x}, want SIGKILL");
    }
    let hits = [0, 1, 2].map(|i| DB_HITS[i].load(Ordering::Acquire));
    let bad = DB_BAD.load(Ordering::Relaxed);
    if hits.contains(&0) || bad != 0 {
        return crate::fail_fmt!(
            "hits entry {} sysretq {} iretq {}, bad PerCpu lookups {bad}",
            hits[0],
            hits[1],
            hits[2]
        );
    }
    Outcome::Ok
}

/// One ring-3 exception case: `code` must end with `wait_signaled(sig)`.
/// A `kvm_only` case needs hardware behaviour TCG does not model and is
/// skipped off KVM. Later slices add rows (C-SUITES).
struct ExcCase {
    name: &'static str,
    code: &'static [u8],
    sig: u32,
    kvm_only: bool,
}

// int3, then exit(1) if it returns.
user_code!(
    USER_INT3,
    "
    int3
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    "
);

// x87 divide by zero with ZM unmasked (FCW 0x037B); `fwait` raises the
// pending error as #MF, since CR0.NE is set.
user_code!(
    USER_X87_MF,
    "
    fninit
    push 0x037B
    fldcw word ptr [rsp]
    fld1
    fldz
    fdivp st(1), st
    fwait
    mov eax, 60
    xor edi, edi
    syscall
    "
);

// SSE divide by zero with MXCSR.ZM clear (0x1D80): #XM, since
// CR4.OSXMMEXCPT is set. TCG does not raise #XM.
user_code!(
    USER_SIMD_XM,
    "
    push 0x1D80
    ldmxcsr dword ptr [rsp]
    mov eax, 1
    cvtsi2ss xmm0, eax
    xorps xmm1, xmm1
    divss xmm0, xmm1
    mov eax, 60
    xor edi, edi
    syscall
    "
);

// RFLAGS.AC set, then a misaligned load: #AC, since CR0.AM is set. TCG
// does not model alignment checks.
user_code!(
    USER_AC_MISALIGNED,
    "
    pushfq
    or qword ptr [rsp], 0x40000
    popfq
    mov eax, dword ptr [rsp+1]
    mov eax, 60
    xor edi, edi
    syscall
    "
);

// Divide by zero.
user_code!(
    USER_DE,
    "
    xor edx, edx
    xor ecx, ecx
    div ecx
    mov eax, 60
    xor edi, edi
    syscall
    "
);

user_code!(
    USER_UD,
    "
    ud2
    "
);

// cli at CPL 3 (IOPL 0): #GP.
user_code!(
    USER_GP,
    "
    cli
    mov eax, 60
    xor edi, edi
    syscall
    "
);

// A load from a user page nothing maps.
user_code!(
    USER_PF,
    "
    mov eax, 0x70000000
    mov rax, qword ptr [rax]
    mov eax, 60
    xor edi, edi
    syscall
    "
);

const EXC_CASES: &[ExcCase] = &[
    ExcCase {
        name: "de",
        code: USER_DE,
        sig: SIGFPE,
        kvm_only: false,
    },
    ExcCase {
        name: "ud",
        code: USER_UD,
        sig: SIGILL,
        kvm_only: false,
    },
    ExcCase {
        name: "gp",
        code: USER_GP,
        sig: SIGSEGV,
        kvm_only: false,
    },
    ExcCase {
        name: "pf",
        code: USER_PF,
        sig: SIGSEGV,
        kvm_only: false,
    },
    ExcCase {
        name: "int3",
        code: USER_INT3,
        sig: SIGTRAP,
        kvm_only: false,
    },
    ExcCase {
        name: "x87_mf",
        code: USER_X87_MF,
        sig: SIGFPE,
        kvm_only: false,
    },
    ExcCase {
        name: "simd_xm",
        code: USER_SIMD_XM,
        sig: SIGFPE,
        kvm_only: true,
    },
    ExcCase {
        name: "ac_misaligned",
        code: USER_AC_MISALIGNED,
        sig: SIGBUS,
        kvm_only: true,
    },
];

pub(crate) fn test_user_exceptions() -> Outcome {
    let kvm = EXC_CASES.iter().any(|c| c.kvm_only) && crate::ktest::on_kvm();
    for case in EXC_CASES {
        if case.kvm_only && !kvm {
            crate::marker!(
                "vibeOS: ktest:   user_exceptions: {} skipped off KVM",
                case.name
            );
            continue;
        }
        let before = settled_frames();
        let st = match user::run(&Image::Code(case.code, DEFAULT), &[case.name]) {
            Ok(st) => st,
            Err(e) => return crate::fail_fmt!("{}: spawn: {}", case.name, e.as_str()),
        };
        if st != wait_signaled(case.sig) {
            return crate::fail_fmt!("{}: status {st:#x}, want signal {}", case.name, case.sig);
        }
        if !user::frames_settle(before) {
            return crate::fail_fmt!("{}: frame leak", case.name);
        }
    }
    Outcome::Ok
}

// Interrupts taken at CPL 3: a child spins in ring 3 on CPU 0 while
// another CPU sends it a device-pool vector and the keyboard's
// (`user_device_irq`), or reschedule IPIs (`user_ipi`). Each body must
// find its `PerCpu` through GS; before the generated stubs the pool and
// keyboard gates skipped the GS step and halted the kernel.

// Spin 100,000 `pause`s, then getpid so a pending SIGKILL acts; forever.
user_code!(
    SPIN_GETPID,
    "
2:
    mov ecx, 100000
1:
    pause
    dec ecx
    jnz 1b
    mov eax, 39
    syscall
    jmp 2b
    "
);

/// CPL-3 hits each vector must reach.
const IRQ_HITS_WANT: u64 = 16;

static IRQ_CHILD: AtomicU64 = AtomicU64::new(0);

static IRQ_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Vectors the sender sends, 0 for none, and each one's CPL-3 hits at start.
static IRQ_VECS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

static IRQ_BASE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

static IRQ_SENT: AtomicBool = AtomicBool::new(false);

static POOL_HITS: AtomicU64 = AtomicU64::new(0);

static POOL_OFF_CPU0: AtomicU64 = AtomicU64::new(0);

/// Pinned to CPU 0, so the child is too (`thread_init::spawn_user`).
fn irq_spawner() {
    let pid = match user::spawn(&Image::Code(SPIN_GETPID, DEFAULT), &["spin_getpid"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    IRQ_CHILD.store(pid, Ordering::Relaxed);
    IRQ_SPAWNED.store(true, Ordering::Release);
}

fn irq_vec(i: usize) -> Option<u8> {
    match IRQ_VECS[i].load(Ordering::Acquire) {
        0 => None,
        v => u8::try_from(v).ok(),
    }
}

fn irq_hits(i: usize) -> u64 {
    irq_vec(i).map_or(u64::MAX, |v| {
        testing::cpl3_hits(v).wrapping_sub(IRQ_BASE[i].load(Ordering::Acquire))
    })
}

/// Sends each vector to CPU 0 about every 50 us until each has
/// `IRQ_HITS_WANT` CPL-3 hits, for at most 5 s.
fn irq_sender() {
    let deadline = time_init::now_ns().saturating_add(5_000_000_000);
    while (irq_hits(0) < IRQ_HITS_WANT || irq_hits(1) < IRQ_HITS_WANT)
        && time_init::now_ns() < deadline
    {
        for v in [irq_vec(0), irq_vec(1)].into_iter().flatten() {
            // A failed send shows as a shortfall in the hit counts.
            let _ = apic_init::send_ipi_cpu(0, v);
        }
        let t = time_init::now_ns().saturating_add(50_000);
        while time_init::now_ns() < t {
            core::hint::spin_loop();
        }
    }
    IRQ_SENT.store(true, Ordering::Release);
}

fn pool_hit() {
    if per_cpu_init::current().cpu_id != 0 {
        POOL_OFF_CPU0.fetch_add(1, Ordering::Relaxed);
    }
    POOL_HITS.fetch_add(1, Ordering::Relaxed);
}

/// Sleep until `pred` holds, for at most `ms`.
fn sleep_until(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Run the ring-3 child on CPU 0 and send it `vecs` (one or two) from
/// another CPU.
fn user_irqs(vecs: &[u8]) -> Outcome {
    let Some(sender_cpu) = crate::ktest::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    for slot in &IRQ_VECS {
        slot.store(0, Ordering::Release);
    }
    IRQ_SPAWNED.store(false, Ordering::Release);
    IRQ_SENT.store(false, Ordering::Release);
    let cpl3_before = cpl3_total();
    crate::ktest::spawn_thread_on("irq_spawner", irq_spawner, 0);
    if !sleep_until(|| IRQ_SPAWNED.load(Ordering::Acquire), 5_000) {
        return Outcome::Fail("spawner did not run");
    }
    let Ok(pid) = u32::try_from(IRQ_CHILD.load(Ordering::Relaxed)) else {
        return Outcome::Fail("spawn");
    };
    // Send only once the child has taken an interrupt in ring 3: its first
    // return (`syscall_init::first_return`) is `user_entry_irq`'s to cover.
    let in_ring3 = sleep_until(|| cpl3_total() != cpl3_before, 5_000);
    if in_ring3 {
        for (i, &v) in vecs.iter().enumerate().take(IRQ_VECS.len()) {
            IRQ_BASE[i].store(testing::cpl3_hits(v), Ordering::Release);
            IRQ_VECS[i].store(u64::from(v), Ordering::Release);
        }
        crate::ktest::spawn_thread_on("irq_sender", irq_sender, sender_cpu);
        // The sender stops itself after 5 s.
        let _ = sleep_until(|| IRQ_SENT.load(Ordering::Acquire), 10_000);
    }
    let hits = [irq_hits(0), irq_hits(1)].map(|h| if h == u64::MAX { 0 } else { h });
    let _ = proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(SIGKILL), 0, 0, 0, 0]);
    let st = user::wait(pid);
    if !in_ring3 {
        return Outcome::Fail("child took no interrupt in ring 3");
    }
    if !IRQ_SENT.load(Ordering::Acquire) {
        return Outcome::Fail("sender did not finish");
    }
    for (&v, &h) in vecs.iter().zip(&hits) {
        if h < IRQ_HITS_WANT {
            return crate::fail_fmt!("vector {v:#x}: {h} CPL-3 hits, want {IRQ_HITS_WANT}");
        }
    }
    if st != wait_signaled(SIGKILL) {
        return crate::fail_fmt!("status {st:#x}, want SIGKILL");
    }
    Outcome::Ok
}

pub(crate) fn test_user_device_irq() -> Outcome {
    let irq = match irq_init::allocate(0) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    if irq_init::set_handler(irq, pool_hit).is_err() {
        let _ = irq_init::free_vector(irq);
        return Outcome::Fail("set_handler");
    }
    let Some(v) = irq_init::vector(irq) else {
        let _ = irq_init::free_vector(irq);
        return Outcome::Fail("no hwirq");
    };
    POOL_HITS.store(0, Ordering::Relaxed);
    POOL_OFF_CPU0.store(0, Ordering::Relaxed);
    let out = user_irqs(&[v, vectors::KBD]);
    let freed = irq_init::free_vector(irq);
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    if freed.is_err() {
        return Outcome::Fail("free_vector");
    }
    if POOL_HITS.load(Ordering::Relaxed) == 0 || POOL_OFF_CPU0.load(Ordering::Relaxed) != 0 {
        return crate::fail_fmt!(
            "pool handler hits {}, off CPU 0 {}",
            POOL_HITS.load(Ordering::Relaxed),
            POOL_OFF_CPU0.load(Ordering::Relaxed)
        );
    }
    Outcome::Ok
}

pub(crate) fn test_user_ipi() -> Outcome {
    user_irqs(&[vectors::IPI_RESCHEDULE])
}

static FK_TARGET: AtomicU32 = AtomicU32::new(0);
static FK_STOP: AtomicBool = AtomicBool::new(false);
static FK_DONE: AtomicBool = AtomicBool::new(false);
/// Floods `FK_TARGET` with reschedule IPIs until `FK_STOP`.
fn force_kernel_ipi_sender() {
    let target = FK_TARGET.load(Ordering::Acquire);
    while !FK_STOP.load(Ordering::Acquire) {
        // A failed send only thins the IPI stream.
        let _ = apic_init::send_ipi_cpu(target, vectors::IPI_RESCHEDULE);
        let t = time_init::now_ns().saturating_add(20_000);
        while time_init::now_ns() < t {
            core::hint::spin_loop();
        }
    }
    FK_DONE.store(true, Ordering::Release);
}

/// `gs::force_kernel` called with IF=1 while another CPU floods this one
/// with IPIs, its window held open: an interrupt between its `mov gs` and
/// its `GS_BASE` write would run at CPL 0 on `GS_BASE` = 0, which the entry
/// stub does not swap, and fault on the first per-CPU access.
pub(crate) fn test_force_kernel_irq_window() -> Outcome {
    let Some(ap) = crate::ktest::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    let Some(me) = x86::cpu_index() else {
        return Outcome::Fail("no cpu index");
    };
    if ap == me {
        return Outcome::Skip("registry on the second CPU");
    }
    if !x86::interrupts_enabled() {
        return Outcome::Fail("IF off in the registry");
    }
    FK_TARGET.store(me, Ordering::Release);
    FK_STOP.store(false, Ordering::Release);
    FK_DONE.store(false, Ordering::Release);
    spawn_thread_on("fk_ipi_sender", force_kernel_ipi_sender, ap);
    arch::catch::arm_force_kernel_window(20);
    let t0 = time_init::now_ns();
    while arch::catch::force_kernel_windows_left() != 0
        && time_init::now_ns().saturating_sub(t0) < 5_000_000_000
    {
        arch::gs::force_kernel();
    }
    let left = arch::catch::force_kernel_windows_left();
    arch::catch::arm_force_kernel_window(0);
    FK_STOP.store(true, Ordering::Release);
    if !spin_until_ns(|| FK_DONE.load(Ordering::Acquire), 5_000_000_000) {
        return Outcome::Fail("IPI sender did not stop");
    }
    if left != 0 {
        return Outcome::Fail("force_kernel did not reach its window");
    }
    let _g = x86::InterruptGuard::enter();
    let Some(cpu) = per_cpu_init::try_current() else {
        return Outcome::Fail("per-CPU area gone");
    };
    if x86::rdmsr(x86::IA32_GS_BASE) != cpu.self_ptr as u64 {
        return Outcome::Fail("GS_BASE is not this CPU's PerCpu");
    }
    Outcome::Ok
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("gdt_selectors", test_gdt_selectors),
    test("star_sysret_layout", test_star_sysret_layout),
    test("sysenter_msrs_zero", msr::sysenter_msrs_zero),
    test("int3_roundtrip", test_int3_roundtrip),
    test("scoped_pf", test_scoped_pf),
    test("gp_catch", test_gp_catch),
    test(
        "idt_set_handler_refuses_fixed",
        test_idt_set_handler_refuses_fixed,
    ),
    test("df_on_ist", test_df_on_ist),
    test("lapic_timer_mode", test_lapic_timer_mode),
    test("lapic_timer_rearm", timer::test_lapic_timer_rearm),
    test("ioapic_pit_gsi_masked", test_ioapic_pit_gsi_masked),
    test("irq_guard_nest", test_irq_guard_nest),
    test("cpu_hardening", test_cpu_hardening),
    test("ac_clear_on_exception", test_ac_clear_on_exception).deadline(30_000),
    test("ac_clear_user_popf", test_ac_clear_user_popf).deadline(30_000),
    test("ist_gs_sign", test_ist_gs_sign).deadline(30_000),
    test("user_exceptions", test_user_exceptions).deadline(30_000),
    test("user_device_irq", test_user_device_irq).deadline(30_000),
    test("user_ipi", test_user_ipi).deadline(30_000),
    test("ipi_icr_writes_if_off", test_ipi_icr_writes_if_off),
    test("cpu_control_regs", control::cpu_control_regs),
    test("cpu_control_clears", control::cpu_control_clears),
    test("catch_ignores_other_cpu", test_catch_ignores_other_cpu),
    test("catch_ignores_user_frame", test_catch_ignores_user_frame).deadline(30_000),
    test("force_kernel_irq_window", test_force_kernel_irq_window).deadline(30_000),
    test("arch_seam_core", test_arch_seam_core),
    test("uaccess_smap_stray_fault", test_uaccess_smap_stray_fault),
    test("uaccess_smep_user_jump", test_uaccess_smep_user_jump),
];
