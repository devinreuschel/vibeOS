//! aarch64 in-guest tests for ROADMAP §11.3 and §11.4.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use vibeos::irq::IrqSpecifier;
use vibeos::kalloc::TryBox;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::mm::asid::{self, AsidAlloc};
use vibeos::paging::{PAGE_SIZE_4K, PageFlags, PhysAddr};
use vibeos::pmm::Frames;
use vibeos::thread::ThreadId;

use crate::arch;
use crate::arch::current::InterruptGuard;
use crate::irq_init;
use crate::ktest::user::{self, DEFAULT, Image, user_code};
use crate::ktest::{Outcome, Test, spawn_thread_on, test};
use crate::kva_init;
use crate::paging_init;
use crate::syscall_init;
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

/// EL1 vector entry must restore x16 (DESIGN §11.5 rule 6: no scratch).
/// Holds x16 live until a timer IRQ, so TCG still takes one at a TB
/// boundary. Fails when the stub saves SP-FRAME as x16.
pub(crate) fn test_el1_x16_survives_irq() -> Outcome {
    if !crate::arch::current::interrupts_enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    let Some(me) = crate::per_cpu_init::cpu(crate::thread_init::current_cpu()) else {
        return Outcome::Fail("no percpu");
    };
    let hz = crate::arch::aarch64::timer::hz();
    if hz == 0 {
        return Outcome::Fail("no cntfrq");
    }
    // Relaxed: a count; pairs with nothing.
    let t0 = me.ticks.load(Ordering::Relaxed);
    let start = crate::arch::aarch64::cpu::cntvct();
    let limit = start.wrapping_add(hz);
    let ticks = me.ticks.as_ptr();
    const CANARY: u64 = 0x1111_2222_3333_4444;
    let mut seen = t0;
    let got: u64;
    // SAFETY: `ticks` is this CPU's remote tick word; the loop only
    // loads it. x16 holds a canary the EL1 stub must restore. established here.
    unsafe {
        asm!(
            "1:",
            "isb",
            "mrs x5, cntvct_el0",
            "cmp x5, x4",
            "b.hs 2f",
            "ldar x2, [x3]",
            "cmp x2, x1",
            "b.eq 1b",
            "2:",
            in("x4") limit,
            in("x3") ticks,
            in("x1") t0,
            inout("x2") seen,
            inout("x16") CANARY => got,
            out("x5") _,
            options(nostack),
        );
    }
    if seen == t0 {
        return Outcome::Fail("no irq while x16 live");
    }
    if got != CANARY {
        return crate::fail_fmt!("x16 {got:#x} want {CANARY:#x}");
    }
    Outcome::Ok
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
static IDLE_CPU: AtomicU32 = AtomicU32::new(u32::MAX);
static IDLE_TID: AtomicU32 = AtomicU32::new(u32::MAX);
static IDLE_WOKE: AtomicU32 = AtomicU32::new(0);

/// Idle-loop hook: IRQs still masked, before `wfi`.
pub(crate) fn idle_pre_wait() {
    // Acquire: pairs with the Release store that arms IDLE_HOOK.
    if IDLE_HOOK.load(Ordering::Acquire) == 0 {
        return;
    }
    // APs sit in this same wfi. Only the CPU that armed the hook
    // claims it, so make_ready runs once and the self-SGI wakes this wfi.
    // Acquire: pairs with the Release store of IDLE_CPU in test_idle_wfi.
    if crate::thread_init::current_cpu() != IDLE_CPU.load(Ordering::Acquire) {
        return;
    }
    // AcqRel: pairs with the arming Release and with losers' Acquire loads.
    if IDLE_HOOK
        .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
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
    IDLE_CPU.store(crate::thread_init::current_cpu(), Ordering::Release);
    IDLE_WOKE.store(0, Ordering::Release);
    timer::disable();
    IDLE_HOOK.store(1, Ordering::Release);
    // halt_if_idle returns before wfi if a leftover wake sits on the
    // runq (a tick during msix_cpu). Drain and retry until the hook
    // runs or CNTVCT says 100 ms.
    let t0 = time_init::now_ns();
    loop {
        thread_init::halt_if_idle();
        thread_init::yield_now();
        if IDLE_WOKE.load(Ordering::Acquire) != 0 {
            break;
        }
        if IDLE_HOOK.load(Ordering::Acquire) == 0 {
            timer::enable();
            IDLE_TID.store(u32::MAX, Ordering::Release);
            return Outcome::Fail("idle wfi did not wake");
        }
        if time_init::now_ns().saturating_sub(t0) > 100_000_000 {
            break;
        }
    }
    timer::enable();
    IDLE_TID.store(u32::MAX, Ordering::Release);
    if IDLE_WOKE.load(Ordering::Acquire) == 0 {
        return Outcome::Fail("idle wfi did not wake");
    }
    Outcome::Ok
}

static SYSREG_WANT: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
static SYSREG_BAD: AtomicU64 = AtomicU64::new(0);
static SYSREG_SEEN: AtomicU64 = AtomicU64::new(0);
static OSLR_BAD: AtomicU64 = AtomicU64::new(0);
static OSLR_SEEN: AtomicU64 = AtomicU64::new(0);
static IDENTITY_BAD: AtomicU64 = AtomicU64::new(0);
static IDENTITY_SEEN: AtomicU64 = AtomicU64::new(0);

const USER_VA: u64 = 0x1_0000;
const ASID_SHIFT: u64 = 48;
const ASID_SPACES: u32 = 1000;
const OSLK: u64 = 1 << 1;
const PERCPU_TICKS_WANT: u64 = 10;
const PERCPU_TICKS_WAIT_NS: u64 = 2_000_000_000;
const TICK_WAIT_NS: u64 = 200_000_000;

fn ap_mask() -> u64 {
    let n = crate::per_cpu_init::cpu_count().min(64) as u32;
    let mut m = 0u64;
    let mut i = 1u32;
    while i < n {
        if crate::per_cpu_init::is_online(i) {
            m |= 1u64 << i;
        }
        i += 1;
    }
    m
}

fn snap_on_ap(_: *mut ()) {
    let v = crate::arch::aarch64::secondary::snapshot_sysregs();
    let id = crate::per_cpu_init::current().cpu_id;
    if id >= 64 {
        return;
    }
    let mut bad = false;
    let mut i = 0;
    while i < 8 {
        if v[i] != SYSREG_WANT[i].load(Ordering::Relaxed) {
            bad = true;
        }
        i += 1;
    }
    if bad {
        SYSREG_BAD.fetch_or(1u64 << id, Ordering::Release);
    }
    SYSREG_SEEN.fetch_or(1u64 << id, Ordering::Release);
}

pub(crate) fn test_sysreg_compare() -> Outcome {
    let want = crate::arch::aarch64::secondary::snapshot_sysregs();
    let mut i = 0;
    while i < 8 {
        SYSREG_WANT[i].store(want[i], Ordering::Relaxed);
        i += 1;
    }
    SYSREG_BAD.store(0, Ordering::SeqCst);
    SYSREG_SEEN.store(0, Ordering::SeqCst);
    let aps = ap_mask();
    if aps == 0 {
        return Outcome::Skip("no AP");
    }
    crate::ipi_init::call_mask(aps, snap_on_ap, core::ptr::null_mut(), true);
    if SYSREG_SEEN.load(Ordering::Acquire) != aps {
        return Outcome::Fail("ap did not snapshot");
    }
    if SYSREG_BAD.load(Ordering::Acquire) != 0 {
        return Outcome::Fail("sysreg mismatch");
    }
    Outcome::Ok
}

fn oslr_on_ap(_: *mut ()) {
    let id = crate::per_cpu_init::current().cpu_id;
    if id >= 64 {
        return;
    }
    if crate::arch::aarch64::cpu::oslsr() & OSLK != 0 {
        OSLR_BAD.fetch_or(1u64 << id, Ordering::Release);
    }
    OSLR_SEEN.fetch_or(1u64 << id, Ordering::Release);
}

pub(crate) fn test_oslsr_clear() -> Outcome {
    if crate::arch::aarch64::cpu::oslsr() & OSLK != 0 {
        return Outcome::Fail("bsp oslk set");
    }
    OSLR_BAD.store(0, Ordering::SeqCst);
    OSLR_SEEN.store(0, Ordering::SeqCst);
    let aps = ap_mask();
    if aps == 0 {
        return Outcome::Skip("no AP");
    }
    crate::ipi_init::call_mask(aps, oslr_on_ap, core::ptr::null_mut(), true);
    if OSLR_SEEN.load(Ordering::Acquire) != aps {
        return Outcome::Fail("ap did not read oslsr");
    }
    if OSLR_BAD.load(Ordering::Acquire) != 0 {
        return Outcome::Fail("ap oslk set");
    }
    Outcome::Ok
}

fn free_pa(pa: u64) {
    if pa == 0 {
        return;
    }
    // SAFETY: `pa` is a buddy page this test allocated and no longer maps.
    // established here.
    unsafe {
        crate::pmm_init::with_buddy(|b| b.free(Frames::from_entry(pa, 0)));
    }
}

fn alloc_zeroed() -> Option<u64> {
    let f = crate::pmm_init::with_buddy(|b| b.alloc(0))?;
    let pa = f.into_entry();
    let va = crate::paging_init::hhdm_offset().wrapping_add(pa);
    // SAFETY: buddy page, HHDM maps it (I14). established here.
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096) };
    Some(pa)
}

pub(crate) fn test_asid_4bit() -> Outcome {
    let ncpus = crate::per_cpu_init::cpu_count().max(2) as u32;
    let Some(alloc) = AsidAlloc::try_boxed(4, ncpus) else {
        return Outcome::Fail("4-bit AsidAlloc");
    };
    let saved = crate::arch::aarch64::cpu::read_ttbr0();
    let cpu = 0u32;
    let flags = PageFlags::empty()
        .with(PageFlags::PRESENT)
        .with(PageFlags::WRITABLE)
        .with(PageFlags::USER);
    let mut i = 0u32;
    while i < ASID_SPACES {
        if crate::ktest::deadline_within(1_000) == Some(true) {
            // SAFETY: restore the empty TTBR0 this CPU had. established here.
            unsafe { crate::arch::aarch64::cpu::write_ttbr0(saved) };
            return Outcome::Fail("asid deadline");
        }
        let Some(l0) = alloc_zeroed() else {
            return Outcome::Fail("asid l0");
        };
        let Some(data) = alloc_zeroed() else {
            free_pa(l0);
            return Outcome::Fail("asid data");
        };
        let Ok(mut tables) = TryBox::try_new([0u64; 8]) else {
            free_pa(data);
            free_pa(l0);
            return Outcome::Fail("asid tables");
        };
        let mut n = 0usize;
        tables[n] = l0;
        n += 1;
        if !crate::arch::aarch64::secondary::map_va(l0, USER_VA, data, &mut tables, &mut n, flags) {
            let mut t = 0;
            while t < n {
                free_pa(tables[t]);
                t += 1;
            }
            free_pa(data);
            return Outcome::Fail("asid map");
        }
        let mut word = 0u64;
        let packed = asid::switch(&alloc, cpu, &mut word);
        if packed == 0 {
            let mut t = 0;
            while t < n {
                free_pa(tables[t]);
                t += 1;
            }
            free_pa(data);
            return Outcome::Fail("asid alloc");
        }
        if alloc.flush_pending(cpu) {
            crate::arch::aarch64::cpu::tlbi_all();
            alloc.clear_flush(cpu);
        }
        let asid = u64::from(alloc.asid_of(packed));
        // SAFETY: `l0` is this space's TTBR0 root; ASID is the allocator's.
        // established here.
        unsafe {
            crate::arch::aarch64::cpu::write_ttbr0(l0 | (asid << ASID_SHIFT));
        }
        let pattern = 0xA5A5_0000u64 | u64::from(i);
        crate::arch::aarch64::cpu::clear_pan();
        // SAFETY: USER_VA is mapped in this TTBR0; PAN is clear. established here.
        unsafe {
            core::ptr::write_volatile(USER_VA as *mut u64, pattern);
            let got = core::ptr::read_volatile(USER_VA as *const u64);
            crate::arch::aarch64::cpu::set_pan();
            crate::arch::aarch64::cpu::write_ttbr0(saved);
            crate::arch::aarch64::cpu::tlbi_all();
            let mut t = 0;
            while t < n {
                free_pa(tables[t]);
                t += 1;
            }
            free_pa(data);
            if got != pattern {
                return crate::fail_fmt!("asid {i} got {got:#x} want {pattern:#x}");
            }
        }
        i += 1;
    }
    Outcome::Ok
}

static SHOOT_VA: AtomicU64 = AtomicU64::new(0);
static SHOOT_REQ: AtomicU64 = AtomicU64::new(0);
static SHOOT_ACK: AtomicU64 = AtomicU64::new(0);
static SHOOT_RESULT: AtomicU64 = AtomicU64::new(0);
const SHOOT_QUIT: u64 = u64::MAX;

fn shoot_prober() {
    let mut seen = 0u64;
    loop {
        // Acquire: pairs with the Release store in `shoot_touch`.
        let req = SHOOT_REQ.load(Ordering::Acquire);
        if req == SHOOT_QUIT {
            SHOOT_ACK.store(SHOOT_QUIT, Ordering::Release);
            return;
        }
        if req != seen {
            let va = SHOOT_VA.load(Ordering::Relaxed);
            // SAFETY: `va` is the test's vmap, or unmapped after vunmap;
            // `catch_fault` recovers. established here.
            let fault = crate::ktest::catch_fault(|| unsafe {
                core::ptr::read_volatile(va as *const u64);
            });
            SHOOT_RESULT.store(if fault.is_some() { 2 } else { 1 }, Ordering::Relaxed);
            seen = req;
            // Release: publishes the result to `shoot_touch`.
            SHOOT_ACK.store(req, Ordering::Release);
        }
        core::hint::spin_loop();
    }
}

fn shoot_touch(req: u64) -> u64 {
    // Release: the mapping change happens before the request.
    SHOOT_REQ.store(req, Ordering::Release);
    if !crate::ktest::spin_until_ns(|| SHOOT_ACK.load(Ordering::Acquire) == req, 2_000_000_000) {
        return 0;
    }
    SHOOT_RESULT.load(Ordering::Relaxed)
}

fn shoot_quit() {
    SHOOT_REQ.store(SHOOT_QUIT, Ordering::Release);
    let _ = crate::ktest::spin_until_ns(
        || SHOOT_ACK.load(Ordering::Acquire) == SHOOT_QUIT,
        2_000_000_000,
    );
}

pub(crate) fn test_tlb_shootdown_remote() -> Outcome {
    let Some(ap) = crate::ktest::second_cpu() else {
        return Outcome::Skip("no AP");
    };
    let Some(frames) = crate::ktest::alloc_frames_owned(0) else {
        return Outcome::Fail("frame alloc");
    };
    let v = match crate::kva_init::vmap(frames) {
        Ok(v) => v,
        Err(_) => return Outcome::Fail("vmap"),
    };
    let va = v.base().as_u64();
    // SAFETY: `va` is this test's vmap. established here.
    unsafe { (va as *mut u64).write_volatile(0xD15EA5E) };
    SHOOT_VA.store(va, Ordering::Relaxed);
    SHOOT_REQ.store(0, Ordering::Relaxed);
    SHOOT_ACK.store(0, Ordering::Relaxed);
    let _prober = crate::ktest::spawn_thread_on("shoot-probe", shoot_prober, ap);
    let r = shoot_touch(1);
    if r != 1 {
        shoot_quit();
        crate::ktest::free_frames_owned(crate::kva_init::vunmap(v));
        return if r == 0 {
            Outcome::Fail("AP probe did not answer")
        } else {
            Outcome::Fail("AP could not read mapped page")
        };
    }
    let frames = crate::kva_init::vunmap(v);
    if shoot_touch(2) != 2 {
        shoot_quit();
        crate::ktest::free_frames_owned(frames);
        return Outcome::Fail("AP did not fault after unmap");
    }
    let v = match crate::kva_init::vmap(frames) {
        Ok(v) => v,
        Err(_) => {
            shoot_quit();
            return Outcome::Fail("remap");
        }
    };
    let va = v.base().as_u64();
    SHOOT_VA.store(va, Ordering::Relaxed);
    // SAFETY: remapped vmap this test owns. established here.
    unsafe { (va as *mut u64).write_volatile(0xD15EA5E) };
    let ok = shoot_touch(3) == 1;
    shoot_quit();
    crate::ktest::free_frames_owned(crate::kva_init::vunmap(v));
    if !ok {
        return Outcome::Fail("AP could not read after remap");
    }
    Outcome::Ok
}

fn identity_on_ap(_: *mut ()) {
    let c = crate::per_cpu_init::current();
    let id = c.cpu_id;
    if id >= 64 {
        return;
    }
    let ok = core::ptr::eq(c.self_ptr, crate::per_cpu_init::gs_self())
        && crate::per_cpu_init::slot_ptr(id) == Some(c.self_ptr)
        && !c.idle.is_null()
        && !crate::arch::current_tcb().is_null()
        && crate::per_cpu_init::cpu(id).is_some_and(|r| core::ptr::eq(r, c.remote));
    if !ok {
        IDENTITY_BAD.fetch_or(1u64 << id, Ordering::Release);
    }
    IDENTITY_SEEN.fetch_or(1u64 << id, Ordering::Release);
}

pub(crate) fn test_per_cpu_identity() -> Outcome {
    let n = crate::per_cpu_init::cpu_count();
    if n == 0 {
        return Outcome::Fail("cpu array empty");
    }
    let bsp_hw = {
        let _g = crate::arch::current::InterruptGuard::enter();
        let bsp = crate::per_cpu_init::current();
        if bsp.cpu_id != 0 {
            return Outcome::Fail("not on bsp");
        }
        if bsp.self_ptr as u64 != bsp as *const _ as u64 {
            return Outcome::Fail("bsp self_ptr");
        }
        if crate::per_cpu_init::gs_self() as u64 != bsp.self_ptr as u64 {
            return Outcome::Fail("bsp tpidr");
        }
        if crate::per_cpu!(cpu_id) != 0 {
            return Outcome::Fail("per_cpu! on bsp");
        }
        bsp.remote.apic_id.load(Ordering::Relaxed)
    };
    if !crate::per_cpu_init::is_online(0) {
        return Outcome::Fail("bsp offline");
    }
    if n < 2 {
        return Outcome::Skip("no AP");
    }
    let mut aps = 0u64;
    let mut i = 1u32;
    while i < n as u32 {
        let Some(c) = crate::ktest::cpu_remote(i) else {
            return Outcome::Fail("missing slot");
        };
        if !c.ready.load(Ordering::Acquire) {
            return Outcome::Fail("ap not ready");
        }
        if c.apic_id.load(Ordering::Relaxed) == bsp_hw {
            return Outcome::Fail("ap hw id");
        }
        if !crate::per_cpu_init::is_online(i) {
            return Outcome::Fail("ap online mask");
        }
        if i < 64 {
            aps |= 1u64 << i;
        }
        i += 1;
    }
    IDENTITY_BAD.store(0, Ordering::SeqCst);
    IDENTITY_SEEN.store(0, Ordering::SeqCst);
    crate::ipi_init::call_mask(aps, identity_on_ap, core::ptr::null_mut(), true);
    if IDENTITY_SEEN.load(Ordering::Acquire) != aps {
        return Outcome::Fail("ap did not run the owner check");
    }
    if IDENTITY_BAD.load(Ordering::Acquire) != 0 {
        return Outcome::Fail("ap owner-only state");
    }
    Outcome::Ok
}

fn remote_ticks(id: u32) -> Option<u64> {
    // Relaxed: a counter read that pairs with no other access.
    crate::ktest::cpu_remote(id).map(|r| r.ticks.load(Ordering::Relaxed))
}

pub(crate) fn test_percpu_ticks_advance() -> Outcome {
    if !crate::arch::current::interrupts_enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    let n = crate::per_cpu_init::cpu_count().min(vibeos::acpi::MAX_CPUS) as u32;
    let mut start = [0u64; vibeos::acpi::MAX_CPUS];
    for id in 0..n {
        if !crate::per_cpu_init::is_online(id) {
            continue;
        }
        let (Some(slot), Some(t)) = (start.get_mut(id as usize), remote_ticks(id)) else {
            return Outcome::Fail("online cpu has no view");
        };
        *slot = t;
    }
    let gained = |id: u32| -> u64 {
        let t0 = start.get(id as usize).copied().unwrap_or(0);
        remote_ticks(id).unwrap_or(t0).wrapping_sub(t0)
    };
    let behind =
        || (0..n).find(|&id| crate::per_cpu_init::is_online(id) && gained(id) < PERCPU_TICKS_WANT);
    crate::ktest::spin_until_ns(|| behind().is_none(), PERCPU_TICKS_WAIT_NS);
    match behind() {
        None => Outcome::Ok,
        Some(id) => crate::fail_fmt!("cpu {id} ticks +{}", gained(id)),
    }
}

fn exercise_fail_cleanup() -> bool {
    match crate::smp_init::alloc_ap_resources(0xFE, 0xFE, false) {
        Ok(a) => {
            crate::smp_init::free_ap_resources(a, false);
            true
        }
        Err(_) => false,
    }
}

pub(crate) fn test_failed_ap_cleanup() -> Outcome {
    if !exercise_fail_cleanup() {
        return Outcome::Fail("warm-up bring-up allocation failed");
    }
    let n0 = crate::ktest::quiescent_free_frames();
    if !exercise_fail_cleanup() {
        return Outcome::Fail("bring-up allocation failed");
    }
    let n1 = crate::ktest::quiescent_free_frames();
    if n0 != n1 {
        crate::marker!("vibeOS: ktest:   frames {n0} -> {n1}");
        Outcome::Fail("failed AP leaked frames")
    } else {
        Outcome::Ok
    }
}

pub(crate) fn test_stalled_ap_leak() -> Outcome {
    if !crate::smp_init::stalled_ap_leaked() {
        return Outcome::Fail("bring-up did not leak a stalled AP");
    }
    let n = crate::per_cpu_init::cpu_count();
    if n < 3 {
        return Outcome::Fail("needs 3 CPUs");
    }
    if crate::per_cpu_init::is_online(1) {
        return Outcome::Fail("stalled AP came online");
    }
    let last = n as u32 - 1;
    if !crate::per_cpu_init::is_online(last) {
        return Outcome::Fail("next AP did not come up");
    }
    let mut online = 0u32;
    let mut i = 0u32;
    while i < n as u32 {
        if crate::per_cpu_init::is_online(i) {
            online += 1;
        }
        i += 1;
    }
    if online + 1 != n as u32 {
        return crate::fail_fmt!("online {online} of {n}, want one hole");
    }
    let n0 = crate::ktest::quiescent_free_frames();
    if !exercise_fail_cleanup() {
        return Outcome::Fail("pre-SIPI free alloc failed");
    }
    let n1 = crate::ktest::quiescent_free_frames();
    if n0 != n1 {
        return crate::fail_fmt!("pre-SIPI free leaked {n0} -> {n1}");
    }
    crate::ktest_info!("stalled AP leaked; online {online}/{n}");
    Outcome::Ok
}

pub(crate) fn test_percpu_remote_view() -> Outcome {
    let n = crate::per_cpu_init::cpu_count();
    if n == 0 {
        return Outcome::Fail("cpu array empty");
    }
    let mut i = 0u32;
    while (i as usize) < n {
        if crate::ktest::cpu_remote(i).is_none() {
            return Outcome::Fail("cpu(i) is None below cpu_count");
        }
        i += 1;
    }
    if crate::ktest::cpu_remote(n as u32).is_some() {
        return Outcome::Fail("cpu(cpu_count) is Some");
    }
    if !crate::arch::current::interrupts_enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    let Some(me) = crate::per_cpu_init::cpu(crate::thread_init::current_cpu()) else {
        return Outcome::Fail("no remote view");
    };
    let t0 = me.ticks.load(Ordering::Relaxed);
    if !crate::ktest::spin_until_ns(|| me.ticks.load(Ordering::Relaxed) != t0, TICK_WAIT_NS) {
        return Outcome::Fail("ticks did not advance with IF on");
    }
    Outcome::Ok
}

/// PL031 wall clock (ROADMAP §11.5): year in 2024..2099, then it advances.
pub(crate) fn test_rtc_offset() -> Outcome {
    let Some(a) = time_init::unix_time_s() else {
        return Outcome::Skip("rtc unread");
    };
    let lo = vibeos::time::unix_from_civil(2024, 1, 1, 0, 0, 0);
    let hi = vibeos::time::unix_from_civil(2100, 1, 1, 0, 0, 0);
    let (Some(lo), Some(hi)) = (lo, hi) else {
        return Outcome::Fail("unix_from_civil");
    };
    if !(lo..hi).contains(&a) {
        return Outcome::Fail("rtc year");
    }
    time_init::busy_wait_ms(20);
    let Some(b) = time_init::unix_time_s() else {
        return Outcome::Fail("rtc lost");
    };
    if b < a {
        return Outcome::Fail("wall clock went backwards");
    }
    Outcome::Ok
}

user_code!(
    EL0_GETPID_10K,
    "
    mov x19, #10000
1:
    mov x8, #172
    svc #0
    subs x19, x19, #1
    b.ne 1b
    mov x0, #0
    mov x8, #93
    svc #0
    "
);

user_code!(
    EL0_SP,
    "
    mov x20, sp
    stur x20, [sp, #-16]
    mov x19, #10000
1:
    mov x8, #172
    svc #0
    subs x19, x19, #1
    b.ne 1b
    ldur x21, [sp, #-16]
    mov x0, #1
    cmp sp, x20
    b.ne 2f
    cmp x21, x20
    b.ne 2f
    mov x0, #0
2:
    mov x8, #93
    svc #0
    "
);

user_code!(
    EL0_TAGGED,
    "
    adr x1, 1f
1:
    mov x2, #0x5a
    lsl x2, x2, #56
    orr x2, x2, x1
    ldr w3, [x2]
    mov x0, #1
    mov x1, x2
    mov x2, #1
    mov x8, #64
    svc #0
    cmn x0, #14
    b.ne 2f
    mov x0, #0
    b 3f
2:
    mov x0, #1
3:
    mov x8, #93
    svc #0
    "
);

user_code!(
    EL0_BRK,
    "
    brk #0
    "
);

user_code!(
    EL0_UDF,
    "
    udf #0
    "
);

user_code!(
    EL0_ODD,
    "
    adr x0, 1f
    add x0, x0, #1
    br x0
1:
    ret
    "
);

user_code!(
    EL0_EXIT0,
    "
    mov x0, #0
    mov x8, #93
    svc #0
    "
);

fn run_code(code: &'static [u8], name: &str) -> Result<u32, Outcome> {
    user::run(&Image::Code(code, DEFAULT), &[name])
        .map_err(|e| crate::fail_fmt!("spawn {name}: {}", e.as_str()))
}

fn exited0(st: u32) -> bool {
    vibeos::proc::wifexited(st) && vibeos::proc::wexitstatus(st) == 0
}

fn signaled(st: u32, sig: u32) -> bool {
    vibeos::proc::wifsignaled(st) && vibeos::proc::wtermsig(st) == sig
}

/// 10,000 `getpid` at EL0; the exit `eret` breakpoint is never taken.
pub(crate) fn test_el0_svc_eret() -> Outcome {
    syscall_init::testing::arm_eret_breakpoint();
    let st = match run_code(EL0_GETPID_10K, "el0_svc") {
        Ok(s) => s,
        Err(e) => {
            syscall_init::testing::disarm_eret_breakpoint();
            return e;
        }
    };
    let hits = syscall_init::testing::eret_bp_hits();
    syscall_init::testing::disarm_eret_breakpoint();
    if !exited0(st) {
        return crate::fail_fmt!("status {st:#x}");
    }
    if hits != 0 {
        return crate::fail_fmt!("eret breakpoint hits {hits}");
    }
    Outcome::Ok
}

/// User SP is intact across 10,000 syscalls and timer preemptions.
pub(crate) fn test_el0_sp() -> Outcome {
    match run_code(EL0_SP, "el0_sp") {
        Ok(st) if exited0(st) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}"),
        Err(e) => e,
    }
}

/// A tagged load works; `write` of the same pointer is `EFAULT`.
pub(crate) fn test_el0_tagged() -> Outcome {
    match run_code(EL0_TAGGED, "el0_tagged") {
        Ok(st) if exited0(st) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}"),
        Err(e) => e,
    }
}

/// Kernel threads load the empty TTBR0 root; a user space loads its own.
pub(crate) fn test_el0_ttbr0() -> Outcome {
    let empty = crate::paging_init::empty_user_root();
    if empty == 0 {
        return Outcome::Fail("no empty TTBR0");
    }
    let live =
        crate::arch::aarch64::cpu::read_ttbr0() & vibeos::arch::aarch64::paging::DESC_ADDR_MASK;
    if live != empty {
        return crate::fail_fmt!("kernel ttbr0 {live:#x} != empty {empty:#x}");
    }
    Outcome::Ok
}

/// `copy_to_user` / `copy_from_user` round-trip a user page.
pub(crate) fn test_el0_uaccess() -> Outcome {
    let Ok(space) = crate::addr_space_init::create() else {
        return Outcome::Fail("create");
    };
    let va = 0x4000_0000u64;
    // SAFETY: `space` is a fresh address space that no CPU has loaded;
    // established here.
    if unsafe {
        crate::addr_space_init::map_anon(
            &space,
            va,
            vibeos::paging::PAGE_SIZE_4K,
            vibeos::addr_space::UserPerms::RW,
        )
    }
    .is_err()
    {
        return Outcome::Fail("map");
    }
    // SAFETY: `space.root` is a user TTBR0 `create` built; established here.
    unsafe { crate::addr_space_init::load_cr3_u64(space.root().as_u64()) };
    let src = *b"pan-ok";
    let mut dst = [0u8; 6];
    let to = crate::arch::aarch64::uaccess::with_window(|| {
        crate::proc::uaccess_init::copy_to_user(va, &src)
    });
    let from = crate::arch::aarch64::uaccess::with_window(|| {
        crate::proc::uaccess_init::copy_from_user(&mut dst, va)
    });
    crate::addr_space_init::load_kernel_cr3();
    drop(space);
    if to.is_err() || from.is_err() || dst != src {
        return Outcome::Fail("copy");
    }
    Outcome::Ok
}

/// `brk`, `udf`, and an odd branch end as SIGTRAP, SIGILL, SIGBUS.
pub(crate) fn test_el0_ring3_signals() -> Outcome {
    let cases = [
        (EL0_BRK, "el0_brk", vibeos::proc::SIGTRAP),
        (EL0_UDF, "el0_udf", vibeos::proc::SIGILL),
        (EL0_ODD, "el0_odd", vibeos::proc::SIGBUS),
    ];
    for (code, name, sig) in cases {
        match run_code(code, name) {
            Ok(st) if signaled(st, sig) => {}
            Ok(st) => return crate::fail_fmt!("{name} status {st:#x} want sig {sig}"),
            Err(e) => return e,
        }
    }
    Outcome::Ok
}

/// A saved ELR of 2^48 kills the process with SIGSEGV; the kernel runs.
pub(crate) fn test_el0_bad_elr() -> Outcome {
    let pid = match user::spawn(&Image::Code(EL0_EXIT0, DEFAULT), &["el0_bad_elr"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    syscall_init::testing::arm_bad_elr();
    let st = user::wait(pid);
    if signaled(st, vibeos::proc::SIGSEGV) {
        Outcome::Ok
    } else {
        crate::fail_fmt!("status {st:#x}")
    }
}

/// `current_tcb` is `mrs SP_EL0`.
pub(crate) fn test_el0_sp_el0_current() -> Outcome {
    let t = crate::arch::current_tcb();
    let via: u64;
    // SAFETY: SP_EL0 holds current at EL1 (ROADMAP §11.6). established here.
    unsafe {
        core::arch::asm!("mrs {0}, sp_el0", out(reg) via, options(nomem, nostack, preserves_flags));
    }
    if t as u64 == via && !t.is_null() {
        Outcome::Ok
    } else {
        crate::fail_fmt!("tcb {t:p} sp_el0 {via:#x}")
    }
}

/// Limine framebuffer over ramfb (ROADMAP §11.5).
pub(crate) fn test_fb_limine() -> Outcome {
    if crate::fb_init::ready() {
        Outcome::Ok
    } else {
        Outcome::Fail("no framebuffer")
    }
}

/// Message-passing and load-buffering with Relaxed stores and no barrier
/// (ROADMAP §11.7). Fails only when a shape completes no iteration.
const WO_ITERS: u64 = 10_000_000;
const WO_BUDGET_NS: u64 = 5 * 60 * 1_000_000_000;
const WO_KIND_MP: u64 = 0;
const WO_KIND_LB: u64 = 1;

static WO_GO: AtomicU64 = AtomicU64::new(0);
static WO_DONE: AtomicU64 = AtomicU64::new(0);
static WO_STOP: AtomicU64 = AtomicU64::new(0);
static WO_KIND: AtomicU64 = AtomicU64::new(0);
static WO_DATA: AtomicU64 = AtomicU64::new(0);
static WO_FLAG: AtomicU64 = AtomicU64::new(0);
static WO_X: AtomicU64 = AtomicU64::new(0);
static WO_Y: AtomicU64 = AtomicU64::new(0);
static WO_OBS_A: AtomicU64 = AtomicU64::new(0);
static WO_OBS_B: AtomicU64 = AtomicU64::new(0);

fn wo_time_up(start: u64) -> bool {
    time_init::now_ns().saturating_sub(start) >= WO_BUDGET_NS
        || crate::ktest::deadline_within(5_000) == Some(true)
}

fn wo_worker() {
    loop {
        if WO_STOP.load(Ordering::Acquire) != 0 {
            return;
        }
        let g = WO_GO.load(Ordering::Acquire);
        if g == 0 || g == WO_DONE.load(Ordering::Relaxed) {
            thread_init::yield_now();
            continue;
        }
        match WO_KIND.load(Ordering::Relaxed) {
            WO_KIND_MP => {
                // Relaxed: the shape under test; pairs with nothing.
                let f = WO_FLAG.load(Ordering::Relaxed);
                let d = WO_DATA.load(Ordering::Relaxed);
                WO_OBS_A.store(f, Ordering::Relaxed);
                WO_OBS_B.store(d, Ordering::Relaxed);
            }
            _ => {
                // Relaxed: the shape under test; pairs with nothing.
                let x = WO_X.load(Ordering::Relaxed);
                WO_Y.store(1, Ordering::Relaxed);
                WO_OBS_A.store(x, Ordering::Relaxed);
            }
        }
        // Release: pairs with the coordinator's Acquire of `WO_DONE`.
        WO_DONE.store(g, Ordering::Release);
    }
}

fn wo_wait(g: u64, start: u64) -> bool {
    while WO_DONE.load(Ordering::Acquire) != g {
        if wo_time_up(start) {
            return false;
        }
        thread_init::yield_now();
    }
    true
}

fn wo_mp(start: u64) -> (u64, u64) {
    // Relaxed: pairs with nothing.
    WO_KIND.store(WO_KIND_MP, Ordering::Relaxed);
    let mut n = 0u64;
    let mut w = 0u64;
    while n < WO_ITERS && !wo_time_up(start) {
        // Relaxed: reset before the Release of `WO_GO`; pairs with nothing.
        WO_DATA.store(0, Ordering::Relaxed);
        WO_FLAG.store(0, Ordering::Relaxed);
        let g = n.saturating_add(1);
        // Release: pairs with the worker's Acquire of `WO_GO`.
        WO_GO.store(g, Ordering::Release);
        // Relaxed: the shape under test; pairs with nothing.
        WO_DATA.store(1, Ordering::Relaxed);
        WO_FLAG.store(1, Ordering::Relaxed);
        if !wo_wait(g, start) {
            break;
        }
        if WO_OBS_A.load(Ordering::Relaxed) == 1 && WO_OBS_B.load(Ordering::Relaxed) == 0 {
            w = w.saturating_add(1);
        }
        n = n.saturating_add(1);
    }
    (n, w)
}

fn wo_lb(start: u64) -> (u64, u64) {
    // Relaxed: pairs with nothing.
    WO_KIND.store(WO_KIND_LB, Ordering::Relaxed);
    let mut n = 0u64;
    let mut w = 0u64;
    while n < WO_ITERS && !wo_time_up(start) {
        // Relaxed: reset before the Release of `WO_GO`; pairs with nothing.
        WO_X.store(0, Ordering::Relaxed);
        WO_Y.store(0, Ordering::Relaxed);
        let g = n.saturating_add(1);
        // Release: pairs with the worker's Acquire of `WO_GO`.
        WO_GO.store(g, Ordering::Release);
        // Relaxed: the shape under test; pairs with nothing.
        let y = WO_Y.load(Ordering::Relaxed);
        WO_X.store(1, Ordering::Relaxed);
        if !wo_wait(g, start) {
            break;
        }
        if y == 0 && WO_OBS_A.load(Ordering::Relaxed) == 0 {
            w = w.saturating_add(1);
        }
        n = n.saturating_add(1);
    }
    (n, w)
}

pub(crate) fn test_weak_order_probe() -> Outcome {
    if crate::per_cpu_init::cpu_count() < 2 {
        return Outcome::Fail("need 2 cpus");
    }
    // Relaxed: this boot's only run; pairs with nothing.
    WO_GO.store(0, Ordering::Relaxed);
    WO_DONE.store(0, Ordering::Relaxed);
    WO_STOP.store(0, Ordering::Relaxed);
    let _worker = spawn_thread_on("wo_probe", wo_worker, 1);
    let start = time_init::now_ns();
    let (mp_n, mp_w) = wo_mp(start);
    let (lb_n, lb_w) = wo_lb(start);
    // Release: pairs with the worker's Acquire of `WO_STOP`.
    WO_STOP.store(1, Ordering::Release);
    crate::ktest_info!("mp iters={mp_n} weak={mp_w} lb iters={lb_n} weak={lb_w}");
    if mp_n == 0 || lb_n == 0 {
        return crate::fail_fmt!("mp {mp_n} lb {lb_n}");
    }
    Outcome::Ok
}

pub(crate) const TESTS: &[Test] = &[
    test("kstack_overflow", test_kstack_overflow),
    test("el1_x16_survives_irq", test_el1_x16_survives_irq),
    test("gic_present", test_gic_present),
    test("now_ns_cntvct", test_now_ns_cntvct),
    test("msix_cpu", test_msix_cpu),
    test("idle_wfi", test_idle_wfi),
    test("sysreg_compare", test_sysreg_compare),
    test("oslsr_clear", test_oslsr_clear),
    test("asid_4bit", test_asid_4bit).deadline(60_000),
    test("tlb_shootdown_remote", test_tlb_shootdown_remote),
    test("per_cpu_identity", test_per_cpu_identity),
    test("percpu_ticks_advance", test_percpu_ticks_advance),
    test("percpu_remote_view", test_percpu_remote_view),
    test("failed_ap_cleanup", test_failed_ap_cleanup),
    test("stalled_ap_leak", test_stalled_ap_leak).opt_in(),
    test("rtc_offset", test_rtc_offset),
    test("fb_limine", test_fb_limine),
    test("el0_svc_eret", test_el0_svc_eret),
    test("el0_sp", test_el0_sp),
    test("el0_tagged", test_el0_tagged),
    test("el0_ttbr0", test_el0_ttbr0),
    test("el0_uaccess", test_el0_uaccess),
    test(
        "uaccess_smap_stray_fault",
        crate::arch::aarch64::ktest_uaccess::test_uaccess_smap_stray_fault,
    ),
    test(
        "uaccess_smep_user_jump",
        crate::arch::aarch64::ktest_uaccess::test_uaccess_smep_user_jump,
    ),
    test(
        "uaccess_syscall_copies",
        crate::arch::aarch64::ktest_uaccess::test_uaccess_syscall_copies,
    )
    .deadline(30_000),
    test(
        "uaccess_readonly_efault",
        crate::arch::aarch64::ktest_uaccess::test_uaccess_readonly_efault,
    )
    .deadline(30_000),
    test(
        "el0_pan_every_cpu",
        crate::arch::aarch64::ktest_el0::test_el0_pan_every_cpu,
    ),
    test(
        "el0_env_every_cpu",
        crate::arch::aarch64::ktest_el0::test_el0_env_every_cpu,
    )
    .deadline(60_000),
    test("el0_sp_el0_current", test_el0_sp_el0_current),
    test(
        "current_at_if1",
        crate::arch::aarch64::ktest_el0::current_at_if1,
    ),
    test(
        "current_migrate_if1",
        crate::arch::aarch64::ktest_el0::current_migrate_if1,
    )
    .deadline(30_000),
    test("el0_ring3_signals", test_el0_ring3_signals),
    test("el0_bad_elr", test_el0_bad_elr),
    test(
        "el0_tls_survive",
        crate::arch::aarch64::ktest_el0::test_el0_tls_survive,
    )
    .deadline(30_000),
    test(
        "el0_tls_yield",
        crate::arch::aarch64::ktest_el0::test_el0_tls_yield,
    )
    .deadline(30_000),
    test(
        "el0_tls_tpidr",
        crate::arch::aarch64::ktest_el0::test_el0_tls_tpidr,
    )
    .deadline(30_000),
    test(
        "el0_fp_no_leak",
        crate::arch::aarch64::ktest_el0::test_el0_fp_no_leak,
    )
    .deadline(60_000),
    test(
        "el0_sve_sigill",
        crate::arch::aarch64::ktest_el0::test_el0_sve_sigill,
    ),
    test("weak_order_probe", test_weak_order_probe)
        .deadline(330_000)
        .opt_in(),
    test("ac_clear_user_popf", skip_ac_clear_user_popf),
    test("ist_gs_sign", skip_ist_gs_sign),
    test("noncanonical_rip_sigsegv", skip_noncanonical_rip),
];

fn skip_ac_clear_user_popf() -> Outcome {
    Outcome::Skip("EL0 cannot write PAN")
}

fn skip_ist_gs_sign() -> Outcome {
    Outcome::Skip("no swapgs")
}

fn skip_noncanonical_rip() -> Outcome {
    Outcome::Skip("no iretq")
}
