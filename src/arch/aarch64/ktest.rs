//! aarch64 in-guest tests for ROADMAP §11.3.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::irq::IrqSpecifier;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::paging::{PAGE_SIZE_4K, PhysAddr};
use vibeos::thread::ThreadId;

use crate::arch;
use crate::arch::current::InterruptGuard;
use crate::irq_init;
use crate::ktest::{Outcome, Test, test};
use crate::kva_init;
use crate::paging_init;
use crate::thread_init;
use crate::time_init;

unsafe extern "C" {
    fn vibeos_fault_on_bad_stack(sp: u64) -> !;
}

global_asm!(
    ".global vibeos_fault_on_bad_stack",
    "vibeos_fault_on_bad_stack:",
    "    mov sp, x0",
    "    str xzr, [x0]",
    "    b .",
);

pub(crate) fn test_kstack_overflow() -> Outcome {
    let Ok(stack) = kva_init::alloc_guarded_stack(DEFAULT_STACK_PAGES) else {
        return Outcome::Fail("guarded stack");
    };
    let poison = stack.guard().as_u64() + 0x800;
    let g = InterruptGuard::enter();
    // SAFETY: `poison` is in the unmapped guard; SP lands there so the
    // vector stub switches to the overflow stack and `catch` longjmps
    // back; established here.
    let caught = arch::catch::catch(|| unsafe {
        vibeos_fault_on_bad_stack(poison);
    });
    drop(g);
    kva_init::free_stack(stack);
    let Some(c) = caught else {
        return Outcome::Fail("did not reach overflow handler");
    };
    let overflow = crate::arch::aarch64::cpu::overflow_sp();
    let lo = overflow.saturating_sub(DEFAULT_STACK_PAGES as u64 * PAGE_SIZE_4K);
    if c.far == poison && c.handler_rsp >= lo && c.handler_rsp <= overflow {
        Outcome::Ok
    } else {
        crate::marker!(
            "vibeOS: ktest:   far={:#x} want={:#x} rsp={:#x} lo={:#x} hi={:#x}",
            c.far,
            poison,
            c.handler_rsp,
            lo,
            overflow
        );
        Outcome::Fail("overflow catch mismatch")
    }
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub(crate) fn gsi_masked(_gsi: u32) -> Option<bool> {
    None
}

pub(crate) fn test_gic_present() -> Outcome {
    if crate::arch::aarch64::gic::chip().is_none() {
        return Outcome::Fail("no gic");
    }
    Outcome::Ok
}

/// IRQs off for 50 ms of `CNTVCT_EL0`; `now_ns` stays within 1%.
pub(crate) fn test_now_ns_cntvct() -> Outcome {
    let hz = crate::arch::aarch64::timer::hz();
    if hz == 0 {
        return Outcome::Fail("no cntfrq");
    }
    let want = hz / 20;
    if want == 0 {
        return Outcome::Fail("cntfrq too small");
    }
    let _g = InterruptGuard::enter();
    let t0 = crate::arch::aarch64::cpu::cntvct();
    let n0 = time_init::now_ns();
    while crate::arch::aarch64::cpu::cntvct().wrapping_sub(t0) < want {
        core::hint::spin_loop();
    }
    let dt = crate::arch::aarch64::cpu::cntvct().wrapping_sub(t0);
    let dn = time_init::now_ns().saturating_sub(n0);
    let Some(expect) = dt
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_div(hz))
    else {
        return Outcome::Fail("scale");
    };
    let err = expect.abs_diff(dn);
    if err.saturating_mul(100) > expect {
        crate::marker!("vibeOS: ktest:   now_ns dt={dt} dn={dn} expect={expect} err={err}");
        return Outcome::Fail("now_ns off >1%");
    }
    Outcome::Ok
}

static MSI_HITS: AtomicU32 = AtomicU32::new(0);

fn on_msi() {
    MSI_HITS.fetch_add(1, Ordering::Release);
}

/// Boot-CPU MSI: compose, map, doorbell write, handler runs.
pub(crate) fn test_msix_cpu() -> Outcome {
    let Some(chip) = crate::arch::aarch64::gic::chip() else {
        return Outcome::Fail("no gic");
    };
    let mut hw = [0u32; 1];
    if chip.alloc_msi(1, 0, &mut hw).is_err() {
        return Outcome::Fail("alloc_msi");
    }
    let Some(&hwirq) = hw.first() else {
        return Outcome::Fail("hwirq");
    };
    let irq = match irq_init::map_wired(IrqSpecifier::Gic { intid: hwirq }) {
        Ok(i) => i,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    if irq_init::set_handler(irq, on_msi).is_err() {
        return Outcome::Fail("handler");
    }
    chip.unmask(hwirq);
    crate::arch::aarch64::gic::map_its_event(0, hwirq);
    let msg = match chip.compose_msi(hwirq, 0) {
        Ok(m) => m,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    if msg.addr == 0 {
        return Outcome::Fail("compose addr 0");
    }
    MSI_HITS.store(0, Ordering::Release);
    if !fire_msi(msg) {
        return Outcome::Fail("doorbell map");
    }
    let t0 = time_init::now_ns();
    while MSI_HITS.load(Ordering::Acquire) == 0 {
        if time_init::now_ns().saturating_sub(t0) > 500_000_000 {
            return Outcome::Fail("no msix");
        }
        core::hint::spin_loop();
    }
    Outcome::Ok
}

fn fire_msi(msg: vibeos::irq::MsiMessage) -> bool {
    let page = msg.addr & !0xFFF;
    let off = msg.addr & 0xFFF;
    // SAFETY: MSI doorbell is device MMIO from MachineDesc (ITS or v2m). established here.
    let Some(va) = (unsafe { paging_init::ioremap(PhysAddr(page), PAGE_SIZE_4K) }) else {
        return false;
    };
    // SAFETY: `va+off` is the composed doorbell. established here.
    unsafe {
        core::ptr::write_volatile((va.as_u64().wrapping_add(off)) as *mut u32, msg.data);
        core::arch::asm!("dsb oshst", options(nostack, preserves_flags));
    }
    true
}

static IDLE_HOOK: AtomicU32 = AtomicU32::new(0);
static IDLE_TID: AtomicU32 = AtomicU32::new(u32::MAX);
static IDLE_WOKE: AtomicU32 = AtomicU32::new(0);

/// Idle-loop hook: IRQs still masked, before `wfi`.
pub(crate) fn idle_pre_wait() {
    if IDLE_HOOK.load(Ordering::Acquire) == 0 {
        return;
    }
    IDLE_HOOK.store(0, Ordering::Release);
    let tid = ThreadId(IDLE_TID.load(Ordering::Acquire));
    if tid.0 != u32::MAX {
        thread_init::make_ready(tid);
    }
    crate::arch::aarch64::ipi::send_reschedule_self();
}

fn idle_wfi_entry() {
    IDLE_WOKE.store(1, Ordering::Release);
}

pub(crate) fn test_idle_wfi() -> Outcome {
    use crate::arch::aarch64::timer;
    let Ok(h) = thread_init::spawn_parked_on("idle-wfi", idle_wfi_entry, 0) else {
        return Outcome::Fail("spawn");
    };
    IDLE_TID.store(h.id().0, Ordering::Release);
    IDLE_WOKE.store(0, Ordering::Release);
    timer::disable();
    IDLE_HOOK.store(1, Ordering::Release);
    thread_init::halt_if_idle();
    thread_init::yield_now();
    timer::enable();
    IDLE_TID.store(u32::MAX, Ordering::Release);
    if IDLE_WOKE.load(Ordering::Acquire) == 0 {
        return Outcome::Fail("idle wfi did not wake");
    }
    Outcome::Ok
}

pub(crate) const TESTS: &[Test] = &[
    test("kstack_overflow", test_kstack_overflow),
    test("gic_present", test_gic_present),
    test("now_ns_cntvct", test_now_ns_cntvct),
    test("msix_cpu", test_msix_cpu),
    test("idle_wfi", test_idle_wfi),
];
